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

//! Parser for Linux's `/proc/net/{tcp,tcp6,udp,udp6}`, and the inode to
//! pid lookup through `/proc/<pid>/fd`.
//!
//! Every row lists one socket with its local and remote address, state,
//! uid and inode, readable by any user. Addresses are hex dumps of the
//! kernel's in-memory `__be32` words printed as host-order integers, so
//! each 8-digit word is decoded with the host's byte order — the tables
//! are only ever read on the machine that wrote them. Ports are plain hex
//! numbers.
//!
//! The format is a stable kernel ABI; a header or row that does not match
//! it refuses the table, the same rule the macOS parser follows, rather
//! than silently leaving a listener out.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::Protocol;

/// `TCP_LISTEN` in the `st` column
const TCP_LISTEN: u8 = 0x0A;

/// One listening row, before its owner is looked up
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub protocol: Protocol,
    pub address: IpAddr,
    pub port: u16,
    pub uid: u32,
    pub inode: u64,
}

/// The listening rows of one table. `v6` says which of the two layouts
/// the address columns use (`tcp6`/`udp6` versus `tcp`/`udp`)
pub(super) fn parse(text: &str, protocol: Protocol, v6: bool) -> Result<Vec<Row>, String> {
    let mut lines = text.lines();
    let header = lines.next().ok_or("empty table")?;
    let columns: Vec<&str> = header.split_whitespace().collect();
    if columns.first() != Some(&"sl") || columns.get(1) != Some(&"local_address") {
        return Err(format!("unrecognised header: {header:?}"));
    }

    let mut rows = Vec::new();
    for line in lines.filter(|l| !l.trim().is_empty()) {
        let row = parse_row(line, protocol, v6).map_err(|e| format!("{e}: {line:?}"))?;
        rows.extend(row);
    }
    Ok(rows)
}

/// `sl local rem st tx:rx tr:when retrnsmt uid timeout inode …`
fn parse_row(line: &str, protocol: Protocol, v6: bool) -> Result<Option<Row>, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [_, local, remote, state, _, _, _, uid, _, inode, ..] = fields.as_slice() else {
        return Err("too few columns".to_string());
    };
    let (address, port) = endpoint(local, v6)?;
    let (remote_address, remote_port) = endpoint(remote, v6)?;
    let state = u8::from_str_radix(state, 16).map_err(|_| "bad state")?;
    let uid: u32 = uid.parse().map_err(|_| "bad uid")?;
    let inode: u64 = inode.parse().map_err(|_| "bad inode")?;

    let listening = port != 0
        && match protocol {
            Protocol::Tcp => state == TCP_LISTEN,
            // Bound, with no remote end fixed by connect()
            Protocol::Udp => remote_port == 0 && remote_address.is_unspecified(),
        };
    Ok(listening.then_some(Row {
        protocol,
        address,
        port,
        uid,
        inode,
    }))
}

/// `0100007F:1F90` → 127.0.0.1:8080; the IPv6 form has 32 address digits
fn endpoint(field: &str, v6: bool) -> Result<(IpAddr, u16), String> {
    let (address, port) = field.split_once(':').ok_or("address without a port")?;
    let port = u16::from_str_radix(port, 16).map_err(|_| "bad port")?;
    let words = address_words(address, if v6 { 4 } else { 1 })?;
    let address = if v6 {
        let mut bytes = [0u8; 16];
        for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            chunk.copy_from_slice(&word.to_ne_bytes());
        }
        IpAddr::V6(Ipv6Addr::from(bytes))
    } else {
        IpAddr::V4(Ipv4Addr::from(words[0].to_ne_bytes()))
    };
    Ok((address, port))
}

fn address_words(hex: &str, count: usize) -> Result<Vec<u32>, String> {
    if hex.len() != count * 8 || !hex.is_ascii() {
        return Err(format!("address {hex:?} is not {count} hex words"));
    }
    (0..count)
        .map(|i| {
            u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16)
                .map_err(|_| format!("address {hex:?} is not hex"))
        })
        .collect()
}

/// `socket:[12345]` → 12345; any other fd target is not a socket
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Which pid holds each wanted socket inode, and whether the scan could
/// see every process. The kernel opens `/proc/<pid>/fd` only for the
/// caller's own processes (or to a privileged caller), so another user's
/// socket has no owner here — unless every wanted inode was found anyway,
/// in which case nothing is missing.
///
/// A socket shared by several processes (a pre-fork server's listening
/// socket) goes to the lowest pid holding it, normally the parent.
#[cfg(target_os = "linux")]
pub(super) fn owners(
    wanted: &std::collections::HashSet<u64>,
) -> (std::collections::HashMap<u64, u32>, super::OwnerCoverage) {
    use std::collections::HashMap;
    use std::io::ErrorKind;

    let mut found: HashMap<u64, u32> = HashMap::new();
    let mut denied = false;
    let mut pids: Vec<u32> = std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    pids.sort_unstable();

    for pid in pids {
        if found.len() == wanted.len() {
            break;
        }
        match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(fds) => {
                for fd in fds.flatten() {
                    let Ok(target) = std::fs::read_link(fd.path()) else {
                        continue;
                    };
                    if let Some(inode) = target.to_str().and_then(socket_inode)
                        && wanted.contains(&inode)
                    {
                        found.entry(inode).or_insert(pid);
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::PermissionDenied => denied = true,
            // Exited between the listing and the read
            Err(_) => {}
        }
    }

    let coverage = if denied && found.len() < wanted.len() {
        super::OwnerCoverage::OwnProcessesOnly
    } else {
        super::OwnerCoverage::AllProcesses
    };
    (found, coverage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from an x86_64 kernel (little-endian, like every host
    /// these tests run on): 127.0.0.1:631 and 0.0.0.0:22 listening, one
    /// established connection, one TIME_WAIT with inode 0
    const TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0277 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 23045 1 0000000000000000 100 0 0 10 0
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 19876 1 0000000000000000 100 0 0 10 0
   2: 0A00020F:0016 0A000202:D2F4 01 00000000:00000000 02:0009E7C8 00000000     0        0 31337 4 0000000000000000 20 4 30 10 -1
   3: 0100007F:9C40 0100007F:1F90 06 00000000:00000000 03:00000F5A 00000000     0        0 0 3 0000000000000000
";

    /// `::1:8080` for uid 1000, and `:::22`
    const TCP6: &str = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 44211 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 19878 1 0000000000000000 100 0 0 10 0
";

    /// Unconnected 0.0.0.0:5353 (listening), a socket connected to
    /// 8.8.8.8:53 (not), and one bound to port 0 (not)
    const UDP: &str = "\
   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  120: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000   107        0 18321 2 0000000000000000 0
  121: 0F02000A:A1B2 08080808:0035 01 00000000:00000000 00:00000000 00000000  1000        0 55120 2 0000000000000000 0
  122: 00000000:0000 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 55121 2 0000000000000000 0
";

    #[test]
    fn tcp_rows_in_listen_are_listeners() {
        let rows = parse(TCP, Protocol::Tcp, false).unwrap();
        assert_eq!(
            rows,
            vec![
                Row {
                    protocol: Protocol::Tcp,
                    address: Ipv4Addr::LOCALHOST.into(),
                    port: 631,
                    uid: 0,
                    inode: 23045,
                },
                Row {
                    protocol: Protocol::Tcp,
                    address: Ipv4Addr::UNSPECIFIED.into(),
                    port: 22,
                    uid: 0,
                    inode: 19876,
                },
            ]
        );
    }

    #[test]
    fn ipv6_addresses_decode_word_by_word() {
        let rows = parse(TCP6, Protocol::Tcp, true).unwrap();
        assert_eq!(rows[0].address, IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(rows[0].port, 8080);
        assert_eq!(rows[0].uid, 1000);
        assert_eq!(rows[1].address, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }

    #[test]
    fn udp_listens_when_bound_and_unconnected() {
        let rows = parse(UDP, Protocol::Udp, false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].port, 5353);
        assert_eq!(rows[0].uid, 107);
    }

    #[test]
    fn a_table_it_does_not_recognise_is_refused() {
        assert!(parse("", Protocol::Tcp, false).is_err());
        assert!(parse("Proto Recv-Q\n", Protocol::Tcp, false).is_err());
        let short_row = "  sl  local_address rem_address st\n   0: 0100007F:0277\n";
        assert!(
            parse(short_row, Protocol::Tcp, false)
                .unwrap_err()
                .contains("too few columns")
        );
        // An IPv4 table read as IPv6 has the wrong address width
        assert!(parse(TCP, Protocol::Tcp, true).is_err());
    }

    #[test]
    fn only_socket_links_carry_an_inode() {
        assert_eq!(socket_inode("socket:[23045]"), Some(23045));
        assert_eq!(socket_inode("pipe:[23045]"), None);
        assert_eq!(socket_inode("/dev/null"), None);
        assert_eq!(socket_inode("socket:[x]"), None);
    }
}
