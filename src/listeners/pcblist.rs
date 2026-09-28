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

//! Parser for XNU's `net.inet.{tcp,udp}.pcblist_n` socket tables.
//!
//! The table opens and closes with a 24-byte `xinpgen` header. Between
//! them, every socket is a run of records, each starting with `u32 len,
//! u32 kind`: `xinpcb_n` (kind 0x10, which starts a socket), `xsocket_n`
//! (0x01), the receive and send `xsockbuf_n` (0x02, 0x04), `xsockstat_n`
//! (0x08) and, for TCP, `xtcpcb_n` (0x20). Records are 8-byte aligned, so
//! the stride is `len` rounded up to 8 — stepping by the raw `len` falls
//! out of step at the first 204-byte `xtcpcb_n`.
//!
//! These are XNU `PRIVATE` structures (`bsd/netinet/in_pcb.h`,
//! `bsd/sys/socketvar.h`, both under `#pragma pack(4)`), so nothing here
//! is taken on trust: every known record's length must equal the length
//! the offsets below were located in, every socket must carry the records
//! it needs, and every attached socket's protocol field must say what the
//! table is. Any mismatch refuses the whole table. Record kinds this
//! parser does not know are stepped over by their length, since a new
//! kind moves nothing it reads. The family field is deliberately NOT
//! checked: live tables carry established and TIME_WAIT sockets whose
//! `xso_family` reads 0 while everything else is populated.
//!
//! The kernel also decides how much of the table a caller sees. Measured
//! on macOS 27.0: a process with an app-bundle identity (an `.app`, or a
//! binary whose signed code directory binds an embedded Info.plist) and a
//! system tool run from a shell get every socket; a bare third-party
//! executable gets only its OWN sockets, not even its children's. The
//! opening header's `xig_count` still counts every socket either way, so
//! [`Parsed::shows_only`] can tell a restricted view from a quiet machine.
//!
//! Offsets were located on macOS 27.0 (26A428), arm64, by binding known
//! sockets and finding their bytes, then checked against every listening
//! socket `netstat -anv` printed. x86_64 shares the layout: `pack(4)`
//! gives 64-bit fields the same 4-byte alignment on both.
//!
//! The table is in host byte order except ports, which are network order.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::{ListenerSnapshot, Protocol};

/// `sizeof(struct xinpgen)`: the opening and closing header
const XINPGEN_LEN: usize = 24;
/// `u32 len` + `u32 kind`: the smallest thing that can be a record
const RECORD_HEADER_LEN: usize = 8;

const XSO_SOCKET: u32 = 0x01;
const XSO_RCVBUF: u32 = 0x02;
const XSO_SNDBUF: u32 = 0x04;
const XSO_STATS: u32 = 0x08;
const XSO_INPCB: u32 = 0x10;
const XSO_TCPCB: u32 = 0x20;

// `xinpcb_n` (104 bytes)
const INP_FPORT: usize = 16; // u16, network order
const INP_LPORT: usize = 18; // u16, network order
const INP_VFLAG: usize = 44; // u8
const INP_FADDR: usize = 48; // 16 bytes: in6_addr, or in_addr_4in6
const INP_LADDR: usize = 64; // 16 bytes: in6_addr, or in_addr_4in6
/// Within an `in_addr_4in6`, the IPv4 address follows 12 bytes of padding
const IN4_IN_4IN6: usize = 12;
const INP_IPV4: u8 = 0x1;
const INP_IPV6: u8 = 0x2;

// `xsocket_n` (104 bytes)
const SO_PROTOCOL: usize = 36; // i32
const SO_UID: usize = 64; // u32
const SO_LAST_PID: usize = 68; // i32
/// What `sotoxsocket_n` leaves when the PCB has no socket attached: the
/// record is written with its length and kind and nothing else
const DETACHED: i32 = 0;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_UDP: i32 = 17;

// `xtcpcb_n` (204 bytes)
const T_STATE: usize = 36; // i32
const TCPS_LISTEN: i32 = 1;

/// The length each known record had when the offsets above were located.
/// Any other length means the structure changed underneath them
fn expected_len(kind: u32) -> Option<usize> {
    match kind {
        XSO_SOCKET | XSO_INPCB => Some(104),
        XSO_RCVBUF | XSO_SNDBUF => Some(32),
        XSO_STATS => Some(136),
        XSO_TCPCB => Some(204),
        _ => None,
    }
}

/// The records of one socket that this parser reads
#[derive(Default)]
struct Run<'a> {
    inpcb: Option<&'a [u8]>,
    socket: Option<&'a [u8]>,
    tcpcb: Option<&'a [u8]>,
}

/// One table, walked
#[derive(Debug, Default)]
pub(super) struct Parsed {
    /// Its listening sockets, with `process` left for the caller to fill
    pub listeners: Vec<ListenerSnapshot>,
    /// `xig_count` from the opening header: every PCB the kernel holds,
    /// whether or not this caller was shown it
    pub header_count: u32,
    /// Sockets actually present in the table
    pub sockets: usize,
    /// Owning pids of every attached socket in the table, listening or not
    pub owners: std::collections::HashSet<u32>,
}

impl Parsed {
    /// Whether the kernel showed `own_pid` only its own sockets: the table
    /// holds fewer sockets than the header counts, and every one it does
    /// hold is ours. An unrestricted read also comes up short when sockets
    /// close mid-walk, but it still holds everyone else's — system
    /// daemons always have sockets — so the second half tells them apart
    pub fn shows_only(&self, own_pid: u32) -> bool {
        (self.sockets as u64) < u64::from(self.header_count)
            && self.owners.iter().all(|pid| *pid == own_pid)
    }

    fn take(&mut self, run: Run, protocol: Protocol) -> Result<(), String> {
        self.sockets += 1;
        if let Some((owner, listener)) = classify(run, protocol)? {
            self.owners.extend(owner);
            self.listeners.extend(listener);
        }
        Ok(())
    }
}

/// Walk one table. `Err` carries why it was refused
pub(super) fn parse(table: &[u8], protocol: Protocol) -> Result<Parsed, String> {
    let opening = read_u32(table, 0).ok_or("table is shorter than its header")? as usize;
    if opening != XINPGEN_LEN {
        return Err(format!(
            "opening header is {opening} bytes, expected {XINPGEN_LEN}"
        ));
    }

    let mut parsed = Parsed {
        header_count: read_u32(table, 4).ok_or("table is shorter than its header")?,
        ..Parsed::default()
    };
    let mut run: Option<Run> = None;
    let mut pos = align8(opening);
    loop {
        let (Some(len), Some(kind)) = (read_u32(table, pos), read_u32(table, pos + 4)) else {
            return Err(format!(
                "table ends at byte {pos} without its closing header"
            ));
        };
        let len = len as usize;
        let record = table
            .get(pos..pos + len)
            .ok_or_else(|| format!("record at byte {pos} runs past the end of the table"))?;
        if len == XINPGEN_LEN {
            // The closing header: the previous socket is complete
            if let Some(done) = run.take() {
                parsed.take(done, protocol)?;
            }
            return Ok(parsed);
        }
        if len < RECORD_HEADER_LEN {
            return Err(format!("record at byte {pos} claims {len} bytes"));
        }

        match expected_len(kind) {
            Some(expected) if expected != len => {
                return Err(format!(
                    "record kind {kind:#x} is {len} bytes, expected {expected}"
                ));
            }
            Some(_) if kind == XSO_INPCB => {
                if let Some(done) = run.replace(Run {
                    inpcb: Some(record),
                    ..Run::default()
                }) {
                    parsed.take(done, protocol)?;
                }
            }
            Some(_) => {
                let current = run
                    .as_mut()
                    .ok_or_else(|| format!("record kind {kind:#x} before the first socket"))?;
                let slot = match kind {
                    XSO_SOCKET => Some(&mut current.socket),
                    XSO_TCPCB => Some(&mut current.tcpcb),
                    // Buffer and statistics records: framing only
                    _ => None,
                };
                if let Some(slot) = slot
                    && slot.replace(record).is_some()
                {
                    return Err(format!("socket carries record kind {kind:#x} twice"));
                }
            }
            // A kind this parser does not know: its length still frames it
            None => {}
        }
        pos += align8(len);
    }
}

/// One socket's owner and, when it is listening, its listener entry.
/// `None` for a PCB with no socket attached: it has no owner and cannot
/// be waiting for connections
#[allow(clippy::type_complexity)]
fn classify(
    run: Run,
    protocol: Protocol,
) -> Result<Option<(Option<u32>, Option<ListenerSnapshot>)>, String> {
    let (Some(inpcb), Some(socket)) = (run.inpcb, run.socket) else {
        return Err("a socket is missing its xsocket_n record".to_string());
    };

    let (expected_protocol, name) = match protocol {
        Protocol::Tcp => (IPPROTO_TCP, "TCP"),
        Protocol::Udp => (IPPROTO_UDP, "UDP"),
    };
    let so_protocol = read_i32(socket, SO_PROTOCOL).ok_or("short xsocket_n")?;
    if so_protocol == DETACHED && socket[RECORD_HEADER_LEN..].iter().all(|b| *b == 0) {
        return Ok(None);
    }
    if so_protocol != expected_protocol {
        return Err(format!(
            "{name} table holds a socket of protocol {so_protocol}"
        ));
    }
    // Negative would be garbage, not an owner
    let owner = read_i32(socket, SO_LAST_PID).and_then(|pid| u32::try_from(pid).ok());

    let port = read_u16_be(inpcb, INP_LPORT).ok_or("short xinpcb_n")?;
    if port == 0 {
        // Created but never bound: nothing to connect to
        return Ok(Some((owner, None)));
    }
    let vflag = *inpcb.get(INP_VFLAG).ok_or("short xinpcb_n")?;
    let address = address_of(inpcb, INP_LADDR, vflag)?;

    let listening = match protocol {
        Protocol::Tcp => {
            let tcpcb = run
                .tcpcb
                .ok_or("a TCP socket is missing its xtcpcb_n record")?;
            read_i32(tcpcb, T_STATE).ok_or("short xtcpcb_n")? == TCPS_LISTEN
        }
        Protocol::Udp => {
            // Bound, with no remote end fixed by connect()
            let foreign_port = read_u16_be(inpcb, INP_FPORT).ok_or("short xinpcb_n")?;
            foreign_port == 0 && address_of(inpcb, INP_FADDR, vflag)?.is_unspecified()
        }
    };
    if !listening {
        return Ok(Some((owner, None)));
    }

    let listener = ListenerSnapshot {
        protocol,
        address,
        port,
        pid: owner,
        process: None,
        run_time_secs: None,
        uid: read_u32(socket, SO_UID),
    };
    Ok(Some((owner, Some(listener))))
}

/// The address stored at `at`, read by the family `inp_vflag` names. A
/// dual-stack socket (both flags) is an IPv6 socket, so its IPv6 form is
/// the one that means something; the same order `netstat` shows as
/// `tcp46`/`udp46` with an IPv6-style wildcard
fn address_of(inpcb: &[u8], at: usize, vflag: u8) -> Result<IpAddr, String> {
    let bytes: [u8; 16] = inpcb
        .get(at..at + 16)
        .and_then(|b| b.try_into().ok())
        .ok_or("short xinpcb_n")?;
    if vflag & INP_IPV6 != 0 {
        Ok(IpAddr::V6(Ipv6Addr::from(bytes)))
    } else if vflag & INP_IPV4 != 0 {
        let v4: [u8; 4] = bytes[IN4_IN_4IN6..].try_into().expect("four bytes");
        Ok(IpAddr::V4(Ipv4Addr::from(v4)))
    } else {
        Err(format!(
            "socket is flagged neither IPv4 nor IPv6 (inp_vflag {vflag:#x})"
        ))
    }
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

fn read_u32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

fn read_i32(buf: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

fn read_u16_be(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(buf.get(at..at + 2)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket to write into a synthetic table, at the offsets the parser
    /// reads — the framing tests below exercise the walk, and the live
    /// tests in the parent module check the offsets against the kernel
    #[derive(Clone)]
    struct Sock {
        v6: bool,
        dual: bool,
        local: IpAddr,
        port: u16,
        foreign: IpAddr,
        foreign_port: u16,
        state: i32,
        pid: i32,
        uid: u32,
    }

    impl Sock {
        fn listening(local: IpAddr, port: u16, pid: i32) -> Self {
            Self {
                v6: local.is_ipv6(),
                dual: false,
                local,
                port,
                foreign: if local.is_ipv6() {
                    Ipv6Addr::UNSPECIFIED.into()
                } else {
                    Ipv4Addr::UNSPECIFIED.into()
                },
                foreign_port: 0,
                state: TCPS_LISTEN,
                pid,
                uid: 501,
            }
        }
    }

    fn put(buf: &mut [u8], at: usize, bytes: &[u8]) {
        buf[at..at + bytes.len()].copy_from_slice(bytes);
    }

    fn put_addr(inpcb: &mut [u8], at: usize, addr: IpAddr) {
        match addr {
            IpAddr::V4(a) => put(inpcb, at + IN4_IN_4IN6, &a.octets()),
            IpAddr::V6(a) => put(inpcb, at, &a.octets()),
        }
    }

    fn record(kind: u32, len: usize) -> Vec<u8> {
        let mut r = vec![0u8; align8(len)];
        put(&mut r, 0, &(len as u32).to_ne_bytes());
        put(&mut r, 4, &kind.to_ne_bytes());
        r
    }

    fn header() -> Vec<u8> {
        counted_header(0)
    }

    /// An `xinpgen` whose `xig_count` claims `count` sockets
    fn counted_header(count: u32) -> Vec<u8> {
        let mut h = vec![0u8; XINPGEN_LEN];
        put(&mut h, 0, &(XINPGEN_LEN as u32).to_ne_bytes());
        put(&mut h, 4, &count.to_ne_bytes());
        h
    }

    fn socket_records(s: &Sock, protocol: Protocol) -> Vec<u8> {
        let mut inpcb = record(XSO_INPCB, 104);
        put(&mut inpcb, INP_FPORT, &s.foreign_port.to_be_bytes());
        put(&mut inpcb, INP_LPORT, &s.port.to_be_bytes());
        inpcb[INP_VFLAG] = match (s.v6, s.dual) {
            (true, true) => INP_IPV4 | INP_IPV6,
            (true, false) => INP_IPV6,
            _ => INP_IPV4,
        };
        put_addr(&mut inpcb, INP_FADDR, s.foreign);
        put_addr(&mut inpcb, INP_LADDR, s.local);

        let mut so = record(XSO_SOCKET, 104);
        let proto = match protocol {
            Protocol::Tcp => IPPROTO_TCP,
            Protocol::Udp => IPPROTO_UDP,
        };
        put(&mut so, SO_PROTOCOL, &proto.to_ne_bytes());
        put(&mut so, SO_UID, &s.uid.to_ne_bytes());
        put(&mut so, SO_LAST_PID, &s.pid.to_ne_bytes());

        let mut out = [
            inpcb,
            so,
            record(XSO_RCVBUF, 32),
            record(XSO_SNDBUF, 32),
            record(XSO_STATS, 136),
        ]
        .concat();
        if protocol == Protocol::Tcp {
            let mut tcpcb = record(XSO_TCPCB, 204);
            put(&mut tcpcb, T_STATE, &s.state.to_ne_bytes());
            out.extend(tcpcb);
        }
        out
    }

    fn table(sockets: &[Sock], protocol: Protocol) -> Vec<u8> {
        let mut t = header();
        for s in sockets {
            t.extend(socket_records(s, protocol));
        }
        t.extend(header());
        t
    }

    fn localhost() -> IpAddr {
        Ipv4Addr::LOCALHOST.into()
    }

    #[test]
    fn reads_listeners_across_the_aligned_tcpcb_stride() {
        // Three sockets in a row: the 204-byte xtcpcb_n pads to 208, so the
        // second and third only parse if the walk steps by the aligned
        // length (the first attempt at this walk stopped after six records)
        let established = Sock {
            state: 4,
            foreign: localhost(),
            foreign_port: 443,
            ..Sock::listening(localhost(), 50_000, 7)
        };
        let t = table(
            &[
                Sock::listening(localhost(), 4226, 15482),
                established,
                Sock::listening(Ipv6Addr::LOCALHOST.into(), 8021, 1),
            ],
            Protocol::Tcp,
        );
        let got = parse(&t, Protocol::Tcp).unwrap().listeners;
        assert_eq!(
            got,
            vec![
                ListenerSnapshot {
                    protocol: Protocol::Tcp,
                    address: localhost(),
                    port: 4226,
                    pid: Some(15482),
                    process: None,
                    run_time_secs: None,
                    uid: Some(501),
                },
                ListenerSnapshot {
                    protocol: Protocol::Tcp,
                    address: Ipv6Addr::LOCALHOST.into(),
                    port: 8021,
                    pid: Some(1),
                    process: None,
                    run_time_secs: None,
                    uid: Some(501),
                },
            ]
        );
    }

    #[test]
    fn a_dual_stack_socket_reports_its_ipv6_wildcard() {
        let dual = Sock {
            dual: true,
            ..Sock::listening(Ipv6Addr::UNSPECIFIED.into(), 7777, 3)
        };
        let got = parse(&table(&[dual], Protocol::Tcp), Protocol::Tcp)
            .unwrap()
            .listeners;
        assert_eq!(got[0].address, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }

    #[test]
    fn udp_listens_when_bound_and_unconnected() {
        let bound = Sock::listening(Ipv4Addr::UNSPECIFIED.into(), 5353, 9);
        let connected = Sock {
            foreign: localhost(),
            foreign_port: 53,
            ..Sock::listening(localhost(), 51_159, 9)
        };
        let unbound = Sock::listening(Ipv4Addr::UNSPECIFIED.into(), 0, 9);
        let got = parse(
            &table(&[bound, connected, unbound], Protocol::Udp),
            Protocol::Udp,
        )
        .unwrap()
        .listeners;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].port, 5353);
        assert_eq!(got[0].protocol, Protocol::Udp);
    }

    #[test]
    fn a_record_of_the_wrong_length_refuses_the_table() {
        let mut t = table(&[Sock::listening(localhost(), 80, 1)], Protocol::Tcp);
        // The xsocket_n that follows the 104-byte xinpcb_n claims 112 bytes
        let at = XINPGEN_LEN + 104;
        put(&mut t, at, &112u32.to_ne_bytes());
        let err = parse(&t, Protocol::Tcp).unwrap_err();
        assert!(err.contains("kind 0x1 is 112 bytes"), "{err}");
    }

    #[test]
    fn a_table_without_its_closing_header_is_refused() {
        let mut t = table(&[Sock::listening(localhost(), 80, 1)], Protocol::Tcp);
        t.truncate(t.len() - XINPGEN_LEN);
        assert!(
            parse(&t, Protocol::Tcp)
                .unwrap_err()
                .contains("closing header")
        );
        // Cut mid-record, too
        t.truncate(t.len() - 10);
        assert!(parse(&t, Protocol::Tcp).is_err());
        assert!(parse(&[], Protocol::Tcp).is_err());
    }

    #[test]
    fn a_tcp_socket_without_its_state_is_refused() {
        let mut records = socket_records(&Sock::listening(localhost(), 80, 1), Protocol::Tcp);
        // Drop the trailing xtcpcb_n (204 bytes, padded to 208)
        records.truncate(records.len() - align8(204));
        let t = [header(), records, header()].concat();
        let err = parse(&t, Protocol::Tcp).unwrap_err();
        assert!(err.contains("missing its xtcpcb_n"), "{err}");
    }

    #[test]
    fn a_table_of_the_wrong_protocol_is_refused() {
        let t = table(&[Sock::listening(localhost(), 53, 1)], Protocol::Udp);
        let err = parse(&t, Protocol::Tcp).unwrap_err();
        assert!(
            err.contains("TCP table holds a socket of protocol 17"),
            "{err}"
        );
    }

    #[test]
    fn records_before_the_first_socket_are_refused() {
        let mut t = header();
        t.extend(record(XSO_SOCKET, 104));
        t.extend(header());
        assert!(
            parse(&t, Protocol::Tcp)
                .unwrap_err()
                .contains("before the first socket")
        );
    }

    #[test]
    fn a_record_kind_it_does_not_know_is_stepped_over() {
        let mut t = header();
        let mut s = socket_records(&Sock::listening(localhost(), 80, 1), Protocol::Tcp);
        // A future kind with an odd length, inside the socket's run
        s.extend(record(0x40, 20));
        t.extend(s);
        t.extend(socket_records(
            &Sock::listening(localhost(), 81, 2),
            Protocol::Tcp,
        ));
        t.extend(header());
        let ports: Vec<u16> = parse(&t, Protocol::Tcp)
            .unwrap()
            .listeners
            .iter()
            .map(|l| l.port)
            .collect();
        assert_eq!(ports, vec![80, 81]);
    }

    #[test]
    fn an_empty_table_is_just_two_headers() {
        let t = [header(), header()].concat();
        let parsed = parse(&t, Protocol::Tcp).unwrap();
        assert!(parsed.listeners.is_empty());
        assert_eq!(parsed.sockets, 0);
        assert!(
            !parsed.shows_only(42),
            "an empty table that counts nothing is just quiet"
        );
    }

    #[test]
    fn a_pcb_with_no_socket_attached_is_skipped_not_refused() {
        let mut detached = socket_records(&Sock::listening(localhost(), 80, 1), Protocol::Tcp);
        // Zero the xsocket_n body, as sotoxsocket_n leaves it for a NULL
        // socket; its header (len, kind) stays
        let so = 104 + RECORD_HEADER_LEN;
        detached[so..104 + 104].fill(0);
        let t = [
            header(),
            detached,
            socket_records(&Sock::listening(localhost(), 81, 2), Protocol::Tcp),
            header(),
        ]
        .concat();
        let parsed = parse(&t, Protocol::Tcp).unwrap();
        assert_eq!(parsed.listeners.len(), 1);
        assert_eq!(parsed.listeners[0].port, 81);
        assert_eq!(parsed.sockets, 2);
        assert_eq!(parsed.owners, [2].into());
    }

    #[test]
    fn a_view_of_only_our_own_sockets_is_told_apart_from_a_quiet_machine() {
        let own = 29_498;
        let mine = Sock::listening(localhost(), 59_575, own as i32);
        let theirs = Sock::listening(localhost(), 631, 1);
        let body = |socks: &[Sock]| -> Vec<u8> {
            socks
                .iter()
                .flat_map(|s| socket_records(s, Protocol::Tcp))
                .collect()
        };

        // What a bare executable is shown: its one socket, of 287 counted
        let restricted = [
            counted_header(287),
            body(std::slice::from_ref(&mine)),
            header(),
        ]
        .concat();
        assert!(parse(&restricted, Protocol::Tcp).unwrap().shows_only(own));

        // A full read that came up short because sockets closed mid-walk
        // still holds other owners
        let short = [counted_header(287), body(&[mine.clone(), theirs]), header()].concat();
        assert!(!parse(&short, Protocol::Tcp).unwrap().shows_only(own));

        // A machine where every socket really is ours
        let all_ours = [counted_header(1), body(&[mine]), header()].concat();
        assert!(!parse(&all_ours, Protocol::Tcp).unwrap().shows_only(own));
    }
}
