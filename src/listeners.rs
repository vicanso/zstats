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

//! The kernel's TCP/UDP socket tables, read only when asked.
//!
//! Two one-shot functions, both deliberately outside [`crate::SystemSnapshot`]
//! and the collector, so the daemon's cost stays exactly what it was:
//!
//! - [`listeners`] — which process is waiting for connections on which
//!   address and port. A list with no baseline; it changes rarely.
//! - [`process_traffic`] — cumulative received and transmitted bytes per
//!   process. On macOS, the `rxbytes` / `txbytes` columns of
//!   `netstat -anv` (TCP and UDP). On Linux, `tcp_info`'s
//!   `bytes_received` / `bytes_sent` (TCP only). One call is not a rate;
//!   the caller diffs two results.
//!
//! Design and measurements: `docs/listeners.md`.
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
//!   privileged — hence [`OwnerCoverage`]. Per-process byte counters are
//!   a separate read: a netlink `SOCK_DIAG` dump of `tcp_info`, then the
//!   same inode walk. UDP has no cumulative counter there.
//! - **Windows** reads the owner-pid socket tables (`GetExtendedTcpTable` /
//!   `GetExtendedUdpTable` with the `*_OWNER_PID_*` classes) through the
//!   `netstat2` crate, which name every socket's owner without elevation.
//!   Its UDP table has no remote end, so a connected UDP socket cannot be
//!   told apart from a listening one there; uids do not exist (`None`).
//! - **Elsewhere** [`listeners`] returns [`CollectError::Unsupported`];
//!   [`crate::Capabilities::listeners`] says so up front.
//!   [`process_traffic`] is macOS and Linux; everywhere else it returns
//!   [`CollectError::Unsupported`], and
//!   [`crate::Capabilities::process_traffic`] says so up front.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::collections::HashMap;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::error::CollectError;

#[cfg(target_os = "linux")]
mod diag;
#[cfg(any(target_os = "macos", test))]
mod pcblist;
#[cfg(any(target_os = "linux", test))]
mod procnet;
#[cfg(any(target_os = "linux", test))]
mod sockdiag;

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
    /// How long the owner has been running, in seconds, as of this call —
    /// the same figure `ProcessSnapshot::run_time_secs` carries, from the
    /// same process entry the name is read from. A server that restarted a
    /// minute ago and one up for weeks hold the same port; this is what
    /// tells them apart. `None` when the OS would not give the start time:
    /// on macOS another user's process has a name but no readable start
    /// (`launchd`, `mDNSResponder`), and sysinfo reports that as 0 — which
    /// would read as "started just now", the one claim it cannot support
    #[serde(default)]
    pub run_time_secs: Option<u64>,
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

/// Cumulative socket bytes per process, as of one [`process_traffic`] call.
/// TCP and UDP on macOS; TCP only on Linux
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTraffic {
    /// Sorted by pid. One row per process that owns at least one socket
    /// this call counts, including a process whose counters are still
    /// zero — zero is a baseline, not "this process has no sockets".
    /// On Linux a process that only speaks UDP has no row
    pub processes: Vec<ProcessTrafficSnapshot>,
    /// Whether an owner could be looked up for every socket. On macOS a
    /// successful call is always [`OwnerCoverage::AllProcesses`]: a view
    /// narrowed to the caller is [`CollectError::Restricted`], not a short
    /// list. On Linux a socket whose `/proc/<pid>/fd` could not be read
    /// stays out of [`Self::processes`], and the coverage says so
    pub coverage: OwnerCoverage,
}

/// One process's sockets, summed
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTrafficSnapshot {
    /// Owning process. On macOS this is `so_last_pid` (who last touched
    /// the socket): a handoff moves that socket's whole cumulative total
    /// onto the new pid in one call. On Linux this is the process holding
    /// the socket's inode, the lowest pid when several share it — the
    /// same rule as [`listeners`]
    pub pid: u32,
    /// The same string [`crate::ProcessSnapshot::name`] carries for that
    /// pid. `None` when the process exited before its name was read
    pub process: Option<String>,
    /// How long the process has been running, in seconds, as of this call.
    /// The same figure [`crate::ProcessSnapshot::run_time_secs`] carries.
    /// A pid reused between two calls starts this over; the previous
    /// process's counters must not be diffed against the new one's.
    /// `None` when the OS would not give the start time
    #[serde(default)]
    pub run_time_secs: Option<u64>,
    /// Cumulative received bytes for the sockets still open.
    ///
    /// On macOS, the sum of `rxbytes` (`netstat -anv`) across the
    /// process's TCP and UDP sockets and the kernel's four traffic
    /// classes. That counter moves by more than the payload `recv`
    /// returned: on macOS 27, reading 1000 bytes back on loopback showed
    /// 2052. On Linux, the sum of `tcpi_bytes_received` across the
    /// process's TCP sockets — payload octets TCP has accepted. UDP is
    /// not included; the kernel's UDP diagnostics carry queue lengths
    /// and no cumulative total. A socket that closes takes its bytes
    /// with it, so the total can fall; a fall is not a negative rate
    pub received_bytes: u64,
    /// Cumulative transmitted bytes, same sockets as [`Self::received_bytes`].
    ///
    /// On macOS this is `txbytes`, which matched the bytes handed to
    /// `send` exactly, on loopback and to a remote host, when the offsets
    /// were located. On Linux it is `tcpi_bytes_sent` (data octets the
    /// stack transmitted, retransmissions included) when `tcp_info` is
    /// long enough to carry it, which it has been since Linux 4.19. An
    /// older `tcp_info` that still has `tcpi_bytes_received` reports
    /// `tcpi_bytes_acked` here instead — octets the peer has acknowledged
    pub transmitted_bytes: u64,
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
        return Err(restricted());
    }
    Ok(finish(tables.listeners, OwnerCoverage::AllProcesses))
}

/// Cumulative received and transmitted bytes for every process that owns
/// a TCP or UDP socket.
///
/// The same read as [`listeners`]: macOS `net.inet.{tcp,udp}.pcblist_n`,
/// nothing kept between calls, and nothing runs unless this is called. The
/// numbers are the kernel's cumulative counters (`netstat -anv`'s
/// `rxbytes` / `txbytes`), summed per owning pid. A rate is the difference
/// of two calls over the wall clock between them.
///
/// Diff per pid, and only against a row whose [`ProcessTrafficSnapshot::run_time_secs`]
/// still names the same process. Skip the pid the first time it appears —
/// the counters are cumulative since its sockets were created, so that
/// first total is not a rate. Skip it again when the total went down: a
/// socket closed and took its bytes along, and the decrease is not traffic
/// in the other direction. A process talking to itself contributes both of
/// its sockets to the one row. The pid is `so_last_pid` (who last touched
/// the socket, the same owner [`listeners`] reports); a handoff moves that
/// socket's whole total onto the new pid.
///
/// Blocking; call it off the UI thread. One call costs what [`listeners`]
/// costs (measured 0.9–4.4 ms of CPU, median about 3.5 ms, calls spaced
/// 1.5 s apart on an M4 Pro) — it is that read, plus adding up the byte
/// fields the listener parse already walks past.
///
/// # Errors
///
/// - [`CollectError::Restricted`] without an app-bundle identity, for the
///   same reason as [`listeners`]: a bare executable is shown only its own
///   sockets, and returning that would read as "nothing else is talking".
/// - [`CollectError::System`] when a table cannot be read or its layout is
///   not the one the offsets were located in.
#[cfg(target_os = "macos")]
pub fn process_traffic() -> Result<ProcessTraffic, CollectError> {
    let tables = read_tables()?;
    if tables.restricted {
        return Err(restricted());
    }
    Ok(finish_traffic(tables.traffic, OwnerCoverage::AllProcesses))
}

/// Cumulative received and transmitted TCP bytes for every process whose
/// sockets this caller can name.
///
/// One netlink `SOCK_DIAG` dump of `tcp_info` for IPv4 and one for IPv6,
/// then the same `/proc/<pid>/fd` walk [`listeners`] uses. Nothing is kept
/// between calls, and nothing runs unless this is called. UDP is absent:
/// the kernel's UDP diagnostics carry queue lengths and no cumulative
/// total, so a process that only speaks UDP has no row.
///
/// [`ProcessTrafficSnapshot::received_bytes`] is `tcpi_bytes_received`
/// (payload octets TCP has accepted).
/// [`ProcessTrafficSnapshot::transmitted_bytes`] is `tcpi_bytes_sent` —
/// data octets transmitted, retransmissions included — when `tcp_info`
/// carries it, which it has since Linux 4.19. An older struct that still
/// reaches `tcpi_bytes_received` reports `tcpi_bytes_acked` (octets the
/// peer has acknowledged) for every row of the call. One kernel means one
/// definition.
///
/// Diff per pid, and only against a row whose
/// [`ProcessTrafficSnapshot::run_time_secs`] still names the same process.
/// Skip the pid the first time it appears — the counters are cumulative
/// since its sockets were created, so that first total is not a rate.
/// Skip it again when the total went down: a socket closed and took its
/// bytes along, and the decrease is not traffic in the other direction. A
/// process talking to itself contributes both of its sockets to the one row.
///
/// A socket whose inode no process we can read holds is left out. Coverage
/// is [`OwnerCoverage::OwnProcessesOnly`] when some `/proc/<pid>/fd` was
/// refused and an inode stayed unowned: another user's process, and on a
/// desktop typically a root daemon, unless this caller is privileged. The
/// counters of those sockets were visible. This is not
/// [`CollectError::Restricted`].
///
/// Blocking; call it off the UI thread. The work is the two dumps plus the
/// fd walk, which stops once every inode has an owner and otherwise reads
/// the fd directory of every process it is allowed to.
///
/// # Errors
///
/// - [`CollectError::Unsupported`] when `tcp_info` is shorter than
///   `tcpi_bytes_received`. A short answer is never returned as a complete
///   total.
/// - [`CollectError::System`] when the dump cannot be read, its layout is
///   not the one this parser walks, or a TCP socket with an inode arrives
///   without `tcp_info`. TIME_WAIT and SYN_RECV entries have inode 0 and
///   no `tcp_info`; those are skipped.
#[cfg(target_os = "linux")]
pub fn process_traffic() -> Result<ProcessTraffic, CollectError> {
    use std::collections::HashSet;

    let flows = diag::tcp_flows()?;
    let wanted: HashSet<u64> = flows.iter().map(|flow| flow.inode).collect();
    let (owners, coverage) = procnet::owners(&wanted);
    let mut totals: HashMap<u32, (u64, u64)> = HashMap::new();
    for flow in flows {
        let Some(&pid) = owners.get(&flow.inode) else {
            continue;
        };
        let entry = totals.entry(pid).or_default();
        entry.0 = entry.0.saturating_add(flow.received_bytes);
        entry.1 = entry.1.saturating_add(flow.transmitted_bytes);
    }
    Ok(finish_traffic(totals, coverage))
}

/// Cumulative per-process socket bytes. This platform publishes no such
/// counter.
///
/// # Errors
///
/// [`CollectError::Unsupported`], always.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_traffic() -> Result<ProcessTraffic, CollectError> {
    Err(CollectError::Unsupported {
        what: "per-process socket byte counters".to_string(),
    })
}

/// A narrowed socket table must not be returned as the whole answer
#[cfg(target_os = "macos")]
fn restricted() -> CollectError {
    CollectError::Restricted {
        message: "macOS shows this process only its own sockets; the socket table \
                  names every owner only to a process with an app-bundle identity"
            .to_string(),
    }
}

/// Both tables' listeners, and whether the kernel narrowed either view to
/// this process
#[cfg(target_os = "macos")]
struct Tables {
    listeners: Vec<ListenerSnapshot>,
    /// `(received, transmitted)` summed per owning pid, across both tables
    traffic: HashMap<u32, (u64, u64)>,
    restricted: bool,
}

#[cfg(target_os = "macos")]
fn read_tables() -> Result<Tables, CollectError> {
    let own_pid = std::process::id();
    let mut tables = Tables {
        listeners: Vec::new(),
        traffic: HashMap::new(),
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
        for flow in parsed.flows {
            let entry = tables.traffic.entry(flow.pid).or_default();
            entry.0 = entry.0.saturating_add(flow.received_bytes);
            entry.1 = entry.1.saturating_add(flow.transmitted_bytes);
        }
    }
    Ok(tables)
}

/// Name the owners and put the rows in pid order
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn finish_traffic(totals: HashMap<u32, (u64, u64)>, coverage: OwnerCoverage) -> ProcessTraffic {
    let owners = process_owners(totals.keys().copied());
    let mut processes: Vec<ProcessTrafficSnapshot> = totals
        .into_iter()
        .map(|(pid, (received_bytes, transmitted_bytes))| {
            let owner = owners.get(&pid);
            ProcessTrafficSnapshot {
                pid,
                process: owner.map(|o| o.name.clone()),
                run_time_secs: owner.and_then(|o| o.run_time_secs),
                received_bytes,
                transmitted_bytes,
            }
        })
        .collect();
    processes.sort_by_key(|row| row.pid);
    ProcessTraffic {
        processes,
        coverage,
    }
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
            run_time_secs: None,
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
        run_time_secs: None,
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
    let owners = process_owners(sockets.iter().filter_map(|s| s.pid));
    for socket in &mut sockets {
        let owner = socket.pid.and_then(|pid| owners.get(&pid));
        socket.process = owner.map(|o| o.name.clone());
        socket.run_time_secs = owner.and_then(|o| o.run_time_secs);
    }
    sockets.sort_by(|a, b| {
        (a.protocol, a.port, a.address, a.pid).cmp(&(b.protocol, b.port, b.address, b.pid))
    });
    sockets.dedup();
    Listeners { sockets, coverage }
}

/// What one process entry says about a listener's owner
struct Owner {
    name: String,
    run_time_secs: Option<u64>,
}

/// Name and run time for exactly these pids, through the same sysinfo
/// lookup the collector uses, so a listener's name and age match the PROC
/// table's. One entry read per pid serves both: the start time rides the
/// same process record the name comes from
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn process_owners(pids: impl Iterator<Item = u32>) -> std::collections::HashMap<u32, Owner> {
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
            let process = system.process(*pid)?;
            let owner = Owner {
                name: process.name().to_string_lossy().into_owned(),
                // A zero start is sysinfo's "could not read it" (measured:
                // pid 1 as an unprivileged user on macOS reads start 0 and
                // run time 0), not a process born this second
                run_time_secs: (process.start_time() > 0).then(|| process.run_time()),
            };
            Some((pid.as_u32(), owner))
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
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use std::io::{Read, Write};
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
            // Our own process: its start is always readable, and recent
            let run_time = socket.run_time_secs.expect("our own start is readable");
            assert!(
                run_time < 24 * 60 * 60,
                "{addr}: {run_time}s is not this test"
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

    /// An owner whose start the OS will not give has no age — never "0",
    /// which would claim it started just now. pid 1 is the case at hand on
    /// macOS; where its start is readable (Linux), it is simply not zero
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_unreadable_start_is_no_age_not_a_new_process() {
        let owners = process_owners([1].into_iter());
        let init = owners.get(&1).expect("pid 1 exists and has a name");
        assert_ne!(init.run_time_secs, Some(0), "{:?}", init.run_time_secs);
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
        // The byte counters come from the same table, so they give the
        // same answer about who is allowed to see it
        match process_traffic() {
            Err(CollectError::Restricted { .. }) => assert!(restricted),
            Ok(traffic) => {
                assert!(!restricted);
                assert!(
                    traffic
                        .processes
                        .iter()
                        .any(|p| p.pid != std::process::id()),
                    "a full view holds other processes' sockets"
                );
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    /// The offsets of `txbytes`: a known send shows up exactly, on the
    /// socket that sent it. Received bytes are the kernel's counter, which
    /// moves by more than the payload, so the check there is only that
    /// they moved
    #[cfg(target_os = "macos")]
    #[test]
    fn transmitted_bytes_match_what_this_process_sent() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let listen_port = listener.local_addr().unwrap().port();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");
        let sent = 1000usize;
        client.write_all(&vec![1u8; sent]).expect("write");
        let mut buf = vec![0u8; sent];
        server.read_exact(&mut buf).expect("read");
        let client_port = client.local_addr().unwrap().port();

        let udp_to = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind udp");
        let udp_from = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind udp");
        let udp_sent = 400usize;
        udp_from
            .send_to(&vec![2u8; udp_sent], udp_to.local_addr().unwrap())
            .expect("sendto");
        udp_to
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut ubuf = [0u8; 512];
        let got = udp_to.recv(&mut ubuf).expect("recv udp");
        assert_eq!(got, udp_sent);
        let udp_port = udp_from.local_addr().unwrap().port();

        let pid = std::process::id();
        let tcp = read_table("net.inet.tcp.pcblist_n").expect("tcp table");
        let tcp = pcblist::parse(&tcp, Protocol::Tcp).expect("tcp parses");
        let sent_on = tcp
            .flows
            .iter()
            .find(|f| f.pid == pid && f.local_port == client_port)
            .unwrap_or_else(|| panic!("client port {client_port} missing"));
        assert_eq!(sent_on.transmitted_bytes, sent as u64);
        let received: u64 = tcp
            .flows
            .iter()
            .filter(|f| f.pid == pid && f.local_port == listen_port)
            .map(|f| f.received_bytes)
            .sum();
        assert!(
            received >= sent as u64,
            "accepted socket received {received}, sent {sent}"
        );

        let udp = read_table("net.inet.udp.pcblist_n").expect("udp table");
        let udp = pcblist::parse(&udp, Protocol::Udp).expect("udp parses");
        let sender = udp
            .flows
            .iter()
            .find(|f| f.pid == pid && f.local_port == udp_port)
            .unwrap_or_else(|| panic!("udp port {udp_port} missing"));
        assert_eq!(sender.transmitted_bytes, udp_sent as u64);
    }

    /// `tcpi_bytes_sent` / `tcpi_bytes_received` count payload octets. The
    /// handshake carries none, so a 1000-byte write that the peer has read
    /// is exactly 1000 on each socket. The pid row may be larger: this
    /// process can hold other sockets
    #[cfg(target_os = "linux")]
    #[test]
    fn transmitted_bytes_match_what_this_process_sent() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");
        let sent = 1000usize;
        client.write_all(&vec![1u8; sent]).expect("write");
        let mut buf = vec![0u8; sent];
        server.read_exact(&mut buf).expect("read");

        let client_inode = inode_of(&client);
        let server_inode = inode_of(&server);
        let flows = diag::tcp_flows().expect("sock_diag");
        let sent_on = flows
            .iter()
            .find(|flow| flow.inode == client_inode)
            .unwrap_or_else(|| panic!("client inode {client_inode} missing"));
        assert_eq!(sent_on.transmitted_bytes, sent as u64);
        let received_on = flows
            .iter()
            .find(|flow| flow.inode == server_inode)
            .unwrap_or_else(|| panic!("server inode {server_inode} missing"));
        assert_eq!(received_on.received_bytes, sent as u64);

        let traffic = process_traffic().expect("process_traffic");
        let mine = traffic
            .processes
            .iter()
            .find(|row| row.pid == std::process::id())
            .expect("this process owns the sockets");
        assert!(mine.transmitted_bytes >= sent as u64);
        assert!(mine.received_bytes >= sent as u64);
        // Still open: dropping them before the dump would take the bytes
        // with the sockets
        let _held = (&client, &server);
    }

    #[cfg(target_os = "linux")]
    fn inode_of(fd: &impl std::os::fd::AsRawFd) -> u64 {
        let link =
            std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).expect("fd target");
        let text = link.to_str().expect("socket link is utf-8");
        procnet::socket_inode(text).unwrap_or_else(|| panic!("not a socket: {text}"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn process_traffic_is_not_on_this_platform() {
        match process_traffic() {
            Err(CollectError::Unsupported { .. }) => {}
            other => panic!("expected Unsupported, got {other:?}"),
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
                run_time_secs: None,
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

    #[test]
    fn process_traffic_serialises_cumulative_bytes() {
        let traffic = ProcessTraffic {
            processes: vec![ProcessTrafficSnapshot {
                pid: 7,
                process: Some("chrome".to_string()),
                run_time_secs: Some(30),
                received_bytes: 100,
                transmitted_bytes: 40,
            }],
            coverage: OwnerCoverage::AllProcesses,
        };
        let json = serde_json::to_value(&traffic).unwrap();
        assert_eq!(json["coverage"], "all_processes");
        assert_eq!(json["processes"][0]["received_bytes"], 100);
        assert_eq!(json["processes"][0]["transmitted_bytes"], 40);
        assert_eq!(json["processes"][0]["run_time_secs"], 30);
        let back: ProcessTraffic = serde_json::from_value(json).unwrap();
        assert_eq!(back, traffic);
    }
}
