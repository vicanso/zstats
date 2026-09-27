// Copyright 2026 Tree xie.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Listening sockets: which process is waiting for connections on which
//! address and port.
//!
//! A one-shot function, deliberately outside [`crate::SystemSnapshot`] and
//! the collector. A listener list is a state with no baseline to diff, it
//! changes rarely, and the caller in view wants it while a person is
//! looking at it — so nothing here runs unless [`listeners`] is called,
//! and the daemon's cost is exactly what it was. Design and measurements:
//! `docs/listeners.md`.
//!
//! Per platform:
//!
//! - **macOS** reads the kernel's own socket tables
//!   (`net.inet.{tcp,udp}.pcblist_n`, what `netstat` reads) through the
//!   `sysctl` crate, which names every socket's owner without root — to a
//!   caller with an app-bundle identity. A bare executable is shown only
//!   its own sockets; that is detected and returned as
//!   [`CollectError::Restricted`] rather than as a near-empty list. The
//!   records are XNU `PRIVATE` layouts, so every record length is checked
//!   against the layout the parser was written for, and a table that does
//!   not match is refused whole rather than half-read.
//! - **Linux** reads `/proc/net/{tcp,tcp6,udp,udp6}` (every socket, any
//!   user) and maps socket inodes to pids through `/proc/<pid>/fd`, which
//!   the kernel opens only for the caller's own processes unless it is
//!   privileged — hence [`OwnerCoverage`].
//! - **Windows** reads the owner-pid socket tables (`GetExtendedTcpTable` /
//!   `GetExtendedUdpTable` with the `*_OWNER_PID_*` classes) through the
//!   `netstat2` crate, which name every socket's owner without elevation.
//!   Its UDP table has no remote end, so a connected UDP socket cannot be
//!   told apart from a listening one there; uids do not exist (`None`).
//! - **Elsewhere** it returns [`CollectError::Unsupported`];
//!   [`crate::Capabilities::listeners`] says so up front.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::error::CollectError;

#[cfg(any(target_os = "macos", test))]
mod pcblist;
#[cfg(any(target_os = "linux", test))]
mod procnet;

/// The result of one [`listeners`] call
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listeners {
    /// Sorted by protocol, then port, then address, then pid. Exact
    /// duplicates (one process holding several `SO_REUSEPORT` sockets on
    /// the same address and port) are collapsed, since this shape has
    /// nothing to tell them apart by. IPv4/IPv6 twins of one port stay
    /// separate: they are different sockets, and merging them is
    /// presentation
    pub sockets: Vec<ListenerSnapshot>,
    /// Whether an owner could be looked up for every socket, or only for
    /// the caller's own processes
    pub coverage: OwnerCoverage,
}

/// One socket waiting for someone else to connect
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerSnapshot {
    pub protocol: Protocol,
    /// The local address. Unspecified (`0.0.0.0`, `::`) means every
    /// interface — reachable from other machines — and loopback means
    /// this machine only: exposure, not the port, is the headline. On
    /// macOS a dual-stack socket (one IPv6 socket accepting IPv4 too,
    /// `netstat`'s `tcp46`) reports its IPv6 address, `::`
    pub address: IpAddr,
    pub port: u16,
    /// The owning process. `None` when the platform would not say — on
    /// Linux, another user's process under
    /// [`OwnerCoverage::OwnProcessesOnly`]. On macOS this is the pid that
    /// last used the socket (`so_last_pid`, what `netstat -v` prints)
    pub pid: Option<u32>,
    /// The owner's process name, the same string `ProcessSnapshot::name`
    /// carries for that pid. `None` when the pid is unknown or the
    /// process exited before its name was read
    pub process: Option<String>,
    /// The socket's user id — known for every socket on macOS and Linux,
    /// including the ones whose process is not
    pub uid: Option<u32>,
}

/// Transport protocol of a listening socket
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// A TCP socket in `LISTEN`
    Tcp,
    /// A UDP socket bound to a local port with no remote address
    Udp,
}

/// How far owner lookup reached
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerCoverage {
    /// Every socket that has an owning process carries it
    AllProcesses,
    /// Only the caller's own processes could be looked up; a socket with
    /// no `pid` belongs to another user, it is not "unknown"
    OwnProcessesOnly,
}

/// Every listening socket this platform lets an unprivileged process see.
///
/// Blocking — it reads the kernel's socket tables and, for owner names, a
/// few process entries — so call it off the UI thread. Nothing is kept
/// between calls.
///
/// # Errors
///
/// - [`CollectError::Unsupported`] off macOS and Linux.
/// - [`CollectError::Restricted`] on macOS when the calling process has no
///   app-bundle identity: the kernel then shows it only its own sockets
///   (measured on macOS 27.0). An `.app` — zstats.app — sees every socket;
///   so does a bare binary whose signed code directory binds an embedded
///   Info.plist.
/// - [`CollectError::System`] when a table cannot be read or, on macOS,
///   its layout is not the one this parser was written for (after an OS
///   update moved it). A layout it does not recognise is never guessed at.
#[cfg(target_os = "macos")]
pub fn listeners() -> Result<Listeners, CollectError> {
    let tables = read_tables()?;
    if tables.restricted {
        return Err(CollectError::Restricted {
            message: "macOS shows this process only its own sockets; the socket table \
                      names every owner only to a process with an app-bundle identity"
                .to_string(),
        });
    }
    Ok(finish(tables.listeners, OwnerCoverage::AllProcesses))
}

/// Both tables' listeners, and whether the kernel narrowed either view to
/// this process
#[cfg(target_os = "macos")]
struct Tables {
    listeners: Vec<ListenerSnapshot>,
    restricted: bool,
}

#[cfg(target_os = "macos")]
fn read_tables() -> Result<Tables, CollectError> {
    let own_pid = std::process::id();
    let mut tables = Tables {
        listeners: Vec::new(),
        restricted: false,
    };
    for (name, protocol) in [
        ("net.inet.tcp.pcblist_n", Protocol::Tcp),
        ("net.inet.udp.pcblist_n", Protocol::Udp),
    ] {
        let table = read_table(name)?;
        let parsed = pcblist::parse(&table, protocol).map_err(|message| CollectError::System {
            message: format!("{name}: socket table layout not recognised: {message}"),
        })?;
        tables.restricted |= parsed.shows_only(own_pid);
        tables.listeners.extend(parsed.listeners);
    }
    Ok(tables)
}

/// See the macOS variant for the contract
#[cfg(target_os = "linux")]
pub fn listeners() -> Result<Listeners, CollectError> {
    use std::collections::HashSet;

    let mut rows = Vec::new();
    for (path, protocol, v6) in [
        ("/proc/net/tcp", Protocol::Tcp, false),
        ("/proc/net/tcp6", Protocol::Tcp, true),
        ("/proc/net/udp", Protocol::Udp, false),
        ("/proc/net/udp6", Protocol::Udp, true),
    ] {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                rows.extend(procnet::parse(&text, protocol, v6).map_err(|message| {
                    CollectError::System {
                        message: format!("{path}: {message}"),
                    }
                })?);
            }
            // A kernel built without IPv6 has no tcp6/udp6: nothing can
            // listen there, which is not an error
            Err(e) if v6 && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(CollectError::System {
                    message: format!("{path}: {e}"),
                });
            }
        }
    }

    // Inode 0 is a socket with no file behind it (orphaned, mid-teardown):
    // nothing can own it
    let wanted: HashSet<u64> = rows.iter().map(|r| r.inode).filter(|i| *i != 0).collect();
    let (owners, coverage) = procnet::owners(&wanted);
    let sockets = rows
        .into_iter()
        .map(|row| ListenerSnapshot {
            protocol: row.protocol,
            address: row.address,
            port: row.port,
            pid: owners.get(&row.inode).copied(),
            process: None,
            uid: Some(row.uid),
        })
        .collect();
    Ok(finish(sockets, coverage))
}

/// See the macOS variant for the contract
#[cfg(target_os = "windows")]
pub fn listeners() -> Result<Listeners, CollectError> {
    use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};

    let sockets = netstat2::get_sockets_info(
        AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
        ProtocolFlags::TCP | ProtocolFlags::UDP,
    )
    .map_err(|e| CollectError::System {
        message: format!("socket tables: {e}"),
    })?;
    let listeners = sockets
        .into_iter()
        .filter_map(|socket| {
            let (protocol, address, port, listening) = match socket.protocol_socket_info {
                ProtocolSocketInfo::Tcp(tcp) => (
                    Protocol::Tcp,
                    tcp.local_addr,
                    tcp.local_port,
                    tcp.state == TcpState::Listen,
                ),
                ProtocolSocketInfo::Udp(udp) => {
                    (Protocol::Udp, udp.local_addr, udp.local_port, true)
                }
            };
            owner_table_row(protocol, address, port, listening, &socket.associated_pids)
        })
        .collect();
    // The owner-pid tables name every socket's owner, elevated or not
    Ok(finish(listeners, OwnerCoverage::AllProcesses))
}

/// One row of Windows' owner-pid socket tables as a listener: a TCP socket
/// in LISTEN, or any bound UDP endpoint — the UDP table carries no remote
/// end, so a connected one cannot be told apart there. Pid 0 is the
/// System Idle Process, which is named as the owner only of sockets whose
/// real owner has gone, so it is no owner at all; pid 4 (System) is a real
/// one (SMB, for instance)
#[cfg(any(target_os = "windows", test))]
fn owner_table_row(
    protocol: Protocol,
    address: IpAddr,
    port: u16,
    listening: bool,
    pids: &[u32],
) -> Option<ListenerSnapshot> {
    (listening && port != 0).then(|| ListenerSnapshot {
        protocol,
        address,
        port,
        pid: pids.iter().copied().filter(|pid| *pid != 0).min(),
        process: None,
        uid: None,
    })
}

/// See the macOS variant for the contract
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn listeners() -> Result<Listeners, CollectError> {
    Err(CollectError::Unsupported {
        what: "listening sockets".to_string(),
    })
}

/// Read one `pcblist_n` table. The kernel's size answer carries some
/// headroom, but a burst of new sockets between the size query and the
/// read still fails the read (ENOMEM) — a retry reads the new size
#[cfg(target_os = "macos")]
fn read_table(name: &str) -> Result<Vec<u8>, CollectError> {
    use sysctl::{Ctl, CtlValue, Sysctl};

    const ATTEMPTS: usize = 3;
    let system = |detail: String| CollectError::System {
        message: format!("{name}: {detail}"),
    };
    let ctl = Ctl::new(name).map_err(|e| system(e.to_string()))?;
    let mut last_error = String::new();
    for _ in 0..ATTEMPTS {
        match ctl.value() {
            Ok(CtlValue::Struct(bytes)) => return Ok(bytes),
            Ok(_) => return Err(system("not a struct-typed sysctl".to_string())),
            Err(e) => last_error = e.to_string(),
        }
    }
    Err(system(last_error))
}

/// Name the owners, then put the list in its documented order
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn finish(mut sockets: Vec<ListenerSnapshot>, coverage: OwnerCoverage) -> Listeners {
    let names = process_names(sockets.iter().filter_map(|s| s.pid));
    for socket in &mut sockets {
        socket.process = socket.pid.and_then(|pid| names.get(&pid).cloned());
    }
    sockets.sort_by(|a, b| {
        (a.protocol, a.port, a.address, a.pid).cmp(&(b.protocol, b.port, b.address, b.pid))
    });
    sockets.dedup();
    Listeners { sockets, coverage }
}

/// Process names for exactly these pids, through the same sysinfo lookup
/// the collector uses, so a listener's name matches the PROC table's
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn process_names(pids: impl Iterator<Item = u32>) -> std::collections::HashMap<u32, String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let mut pids: Vec<Pid> = pids.map(Pid::from_u32).collect();
    pids.sort_unstable();
    pids.dedup();
    if pids.is_empty() {
        return Default::default();
    }
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        false,
        ProcessRefreshKind::nothing(),
    );
    pids.iter()
        .filter_map(|pid| {
            let name = system.process(*pid)?.name().to_string_lossy().into_owned();
            Some((pid.as_u32(), name))
        })
        .collect()
}

#[cfg(test)]
mod owner_table_tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn a_listening_tcp_row_keeps_its_owner() {
        let row = owner_table_row(Protocol::Tcp, Ipv4Addr::UNSPECIFIED.into(), 445, true, &[4])
            .expect("listening");
        assert_eq!(row.pid, Some(4), "System is a real owner");
        assert_eq!(row.uid, None, "Windows has no uids");
        assert_eq!(row.process, None, "names are filled later");
    }

    #[test]
    fn only_listening_rows_on_a_real_port_are_listeners() {
        let any = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        assert!(owner_table_row(Protocol::Tcp, any, 443, false, &[900]).is_none());
        assert!(owner_table_row(Protocol::Udp, any, 0, true, &[900]).is_none());
        assert!(owner_table_row(Protocol::Udp, any, 5353, true, &[900]).is_some());
    }

    #[test]
    fn the_idle_process_is_no_owner() {
        let row = owner_table_row(Protocol::Tcp, Ipv4Addr::LOCALHOST.into(), 80, true, &[0]);
        assert_eq!(row.unwrap().pid, None);
    }
}

#[cfg(all(
    test,
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream, UdpSocket};

    use super::*;

    #[cfg(unix)]
    fn current_uid() -> u32 {
        let out = std::process::Command::new("id")
            .arg("-u")
            .output()
            .expect("id runs");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("numeric uid")
    }

    fn find(
        list: &Listeners,
        protocol: Protocol,
        address: IpAddr,
        port: u16,
    ) -> Option<&ListenerSnapshot> {
        list.sockets
            .iter()
            .find(|s| s.protocol == protocol && s.address == address && s.port == port)
    }

    /// Every listener this process can be shown, named. On macOS this
    /// bypasses the restriction check on purpose: a test binary is a bare
    /// executable, so the kernel shows it only its own sockets — which
    /// are exactly the ones these tests bind, and the offsets are what is
    /// under test
    #[cfg(target_os = "macos")]
    fn visible() -> Listeners {
        let tables = read_tables().expect("the socket tables parse");
        finish(tables.listeners, OwnerCoverage::AllProcesses)
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn visible() -> Listeners {
        listeners().expect("the socket tables parse")
    }

    /// The live check the design asks for: sockets this test binds come
    /// back with this test's own pid, uid and address. A layout change on
    /// the reference platform fails here loudly instead of reading garbage
    #[test]
    fn sockets_this_process_binds_come_back_with_its_pid() {
        let v4_local = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind 127.0.0.1");
        let v4_any = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind 0.0.0.0");
        let v6_local = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).ok();
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind udp");
        // A connected UDP socket and an accepted TCP stream are not
        // listeners, whatever their local ports
        let connected_udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind udp");
        connected_udp
            .connect(udp.local_addr().unwrap())
            .expect("connect udp");
        let client = TcpStream::connect(v4_local.local_addr().unwrap()).expect("connect tcp");

        let list = visible();
        let pid = std::process::id();
        #[cfg(unix)]
        let uid = Some(current_uid());
        #[cfg(windows)]
        let uid = None;

        let mut expected = vec![
            (Protocol::Tcp, v4_local.local_addr().unwrap()),
            (Protocol::Tcp, v4_any.local_addr().unwrap()),
            (Protocol::Udp, udp.local_addr().unwrap()),
        ];
        if let Some(l) = &v6_local {
            expected.push((Protocol::Tcp, l.local_addr().unwrap()));
        }
        for (protocol, addr) in expected {
            let socket = find(&list, protocol, addr.ip(), addr.port())
                .unwrap_or_else(|| panic!("{protocol:?} {addr} missing from {list:#?}"));
            assert_eq!(socket.pid, Some(pid), "{addr}");
            assert_eq!(socket.uid, uid, "{addr}");
            assert!(
                socket.process.as_deref().is_some_and(|n| !n.is_empty()),
                "{addr} has no process name"
            );
        }

        // Windows' UDP table has no remote end to tell a connected socket by
        #[cfg(unix)]
        {
            let connected = connected_udp.local_addr().unwrap();
            assert!(find(&list, Protocol::Udp, connected.ip(), connected.port()).is_none());
        }
        let stream = client.local_addr().unwrap();
        assert!(find(&list, Protocol::Tcp, stream.ip(), stream.port()).is_none());

        let mut sorted = list.sockets.clone();
        sorted.sort_by(|a, b| {
            (a.protocol, a.port, a.address, a.pid).cmp(&(b.protocol, b.port, b.address, b.pid))
        });
        assert_eq!(sorted, list.sockets, "documented order");
    }

    /// A view narrowed to this process must never come back as a list: it
    /// would read as "nothing else is listening". Whichever view this test
    /// binary is given, `listeners()` has to say so honestly
    #[cfg(target_os = "macos")]
    #[test]
    fn a_view_narrowed_to_this_process_is_an_error_not_a_list() {
        // Holding a socket makes the restricted view non-empty, the case
        // most likely to be mistaken for a real answer
        let _held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let restricted = read_tables().expect("the socket tables parse").restricted;
        match listeners() {
            Err(CollectError::Restricted { .. }) => assert!(restricted),
            Ok(list) => {
                assert!(!restricted);
                assert!(
                    list.sockets
                        .iter()
                        .any(|s| s.pid != Some(std::process::id())),
                    "a full view holds other processes' sockets"
                );
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn serialises_in_its_documented_vocabulary() {
        let list = Listeners {
            sockets: vec![ListenerSnapshot {
                protocol: Protocol::Udp,
                address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                port: 5353,
                pid: None,
                process: None,
                uid: Some(65),
            }],
            coverage: OwnerCoverage::OwnProcessesOnly,
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["coverage"], "own_processes_only");
        assert_eq!(json["sockets"][0]["protocol"], "udp");
        assert_eq!(json["sockets"][0]["address"], "::");
        let back: Listeners = serde_json::from_value(json).unwrap();
        assert_eq!(back, list);
    }
}
