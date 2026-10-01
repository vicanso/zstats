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

//! Parser for Linux's `SOCK_DIAG` `tcp_info` dump, the table `ss -i` reads.
//!
//! A request is one netlink message: `nlmsghdr` plus `inet_diag_req_v2`
//! asking for every TCP state and the `INET_DIAG_INFO` extension. The
//! reply is a multipart dump of `inet_diag_msg` (inode at byte 68) plus
//! netlink attributes. `INET_DIAG_INFO`'s payload is `struct tcp_info`.
//! The kernel may insert a padding attribute in front of it
//! (`nla_reserve_64bit`); unknown attributes are skipped.
//!
//! `tcpi_bytes_acked` / `tcpi_bytes_received` / `tcpi_bytes_sent` sit at
//! fixed offsets. The uapi struct only ever grows by appending, and these
//! three have not moved since `tcpi_bytes_sent` arrived in Linux 4.19
//! (checked against the 4.19, 5.4, 6.1 and 6.12 headers). A `tcp_info`
//! shorter than `tcpi_bytes_received` is a kernel too old to answer. One
//! that reaches `tcpi_bytes_received` but not `tcpi_bytes_sent` reports
//! `tcpi_bytes_acked` as the transmitted total — every socket in one dump
//! comes from one kernel, so the choice is the same for the whole call.
//!
//! TIME_WAIT and SYN_RECV entries carry inode 0 and no `tcp_info`. They
//! are skipped: the bytes left with the socket that closed, which is the
//! same rule as a total that fell between two calls.
//!
//! Numeric fields are host byte order. The dump is only ever parsed on
//! the machine that wrote it.

/// `SOCK_DIAG_BY_FAMILY`
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLMSG_NOOP: u16 = 0x1;
const NLMSG_ERROR: u16 = 0x2;
const NLMSG_DONE: u16 = 0x3;
const NLM_F_REQUEST: u16 = 0x01;
/// `NLM_F_ROOT | NLM_F_MATCH`
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_DUMP_INTR: u16 = 0x10;

const NLMSG_HDRLEN: usize = 16;
/// `sizeof(inet_diag_req_v2)`: 8 bytes of header plus a 48-byte sockid
const REQ_BODY: usize = 56;
/// `sizeof(inet_diag_msg)`
const DIAG_MSG_LEN: usize = 72;
/// `idiag_inode` within `inet_diag_msg`
const INODE_AT: usize = 68;

const IPPROTO_TCP: u8 = 6;
/// `1 << (INET_DIAG_INFO - 1)`. `INET_DIAG_INFO` itself, as an attribute
/// type, is 2 — the same number, for a different reason
const EXT_INFO: u8 = 1 << 1;
const INET_DIAG_INFO: u16 = 2;
/// `NLA_F_NESTED | NLA_F_NET_BYTEORDER`: flags the kernel may set in the
/// attribute type word
const NLA_TYPE_MASK: u16 = !((1 << 15) | (1 << 14));

/// Within `struct tcp_info`. See the module comment for why these are fixed
const BYTES_ACKED_AT: usize = 120;
const BYTES_RECEIVED_AT: usize = 128;
const BYTES_SENT_AT: usize = 200;
const MIN_RECEIVED: usize = BYTES_RECEIVED_AT + 8;
const MIN_SENT: usize = BYTES_SENT_AT + 8;

/// One TCP socket's cumulative counters. `inode` is what
/// `/proc/<pid>/fd` names the socket, so the caller can find its owner
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TcpFlow {
    pub inode: u64,
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
}

/// Why [`parse`] refused a buffer
#[derive(Debug)]
pub(super) enum ParseError {
    /// The bytes are not a walkable netlink dump
    Malformed { message: String },
    /// `tcp_info` predates `tcpi_bytes_received`
    NoByteCounters { bytes: usize },
}

/// One `recv` of a dump
#[derive(Debug)]
pub(super) struct DumpPart {
    pub flows: Vec<TcpFlow>,
    /// `NLMSG_DONE` was in this buffer. The dump is finished
    pub done: bool,
    /// The kernel set `NLM_F_DUMP_INTR`: the table changed mid-walk
    pub interrupted: bool,
    /// `NLMSG_ERROR` payload, a negative errno. `None` when the kernel
    /// did not refuse the dump
    pub errno: Option<i32>,
}

/// The dump request for one address family (`AF_INET` is 2, `AF_INET6`
/// is 10). `seq` is echoed on every reply so a stale message is visible
pub(super) fn request(family: u8, seq: u32) -> [u8; NLMSG_HDRLEN + REQ_BODY] {
    let mut buf = [0u8; NLMSG_HDRLEN + REQ_BODY];
    let len = buf.len() as u32;
    put_u32(&mut buf, 0, len);
    put_u16(&mut buf, 4, SOCK_DIAG_BY_FAMILY);
    put_u16(&mut buf, 6, NLM_F_REQUEST | NLM_F_DUMP);
    put_u32(&mut buf, 8, seq);
    buf[16] = family;
    buf[17] = IPPROTO_TCP;
    buf[18] = EXT_INFO;
    // Every TCP state. Zero would match nothing
    put_u32(&mut buf, 20, u32::MAX);
    // `idiag_cookie` is the last 8 bytes of the 48-byte sockid, which
    // starts at byte 24. `INET_DIAG_NOCOOKIE` is all-ones
    put_u32(&mut buf, 64, u32::MAX);
    put_u32(&mut buf, 68, u32::MAX);
    buf
}

/// Walk one `recv` buffer. `seq` is the request's sequence; a message
/// carrying another one is refused rather than mixed in
pub(super) fn parse(seq: u32, buf: &[u8]) -> Result<DumpPart, ParseError> {
    let mut part = DumpPart {
        flows: Vec::new(),
        done: false,
        interrupted: false,
        errno: None,
    };
    let mut pos = 0;
    if buf.is_empty() {
        return Err(malformed("empty netlink reply"));
    }
    while pos + NLMSG_HDRLEN <= buf.len() && !part.done && part.errno.is_none() {
        let len = read_u32(buf, pos)? as usize;
        if len < NLMSG_HDRLEN {
            return Err(malformed(format!(
                "message at byte {pos} claims {len} bytes"
            )));
        }
        if pos + len > buf.len() {
            return Err(malformed(format!(
                "message at byte {pos} runs past the end of the reply"
            )));
        }
        let kind = read_u16(buf, pos + 4)?;
        let flags = read_u16(buf, pos + 6)?;
        let msg_seq = read_u32(buf, pos + 8)?;
        if msg_seq != seq {
            return Err(malformed(format!(
                "message sequence {msg_seq} is not the request's {seq}"
            )));
        }
        let msg = &buf[pos..pos + len];
        match kind {
            NLMSG_DONE => {
                part.done = true;
                part.interrupted = flags & NLM_F_DUMP_INTR != 0;
            }
            NLMSG_ERROR => {
                // An errno of 0 is an acknowledgement, not a failure
                let err = read_i32(msg, NLMSG_HDRLEN)?;
                if err != 0 {
                    part.errno = Some(err);
                }
            }
            NLMSG_NOOP => {}
            SOCK_DIAG_BY_FAMILY => part.flows.extend(flow_of(msg)?),
            // A kind this parser does not know: its length still frames it
            _ => {}
        }
        pos += align4(len);
    }
    Ok(part)
}

/// One socket, or nothing when the entry is a TIME_WAIT / SYN_RECV with
/// inode 0. A real socket missing `tcpi_bytes_received` refuses the dump
fn flow_of(msg: &[u8]) -> Result<Option<TcpFlow>, ParseError> {
    if msg.len() < NLMSG_HDRLEN + DIAG_MSG_LEN {
        return Err(malformed("inet_diag_msg is short"));
    }
    let inode = u64::from(read_u32(msg, NLMSG_HDRLEN + INODE_AT)?);
    if inode == 0 {
        return Ok(None);
    }
    let info = tcp_info(msg)?
        .ok_or_else(|| malformed(format!("TCP socket inode {inode} is missing its tcp_info")))?;
    if info.len() < MIN_RECEIVED {
        return Err(ParseError::NoByteCounters { bytes: info.len() });
    }
    let received_bytes = read_u64(info, BYTES_RECEIVED_AT)?;
    let transmitted_bytes = if info.len() >= MIN_SENT {
        read_u64(info, BYTES_SENT_AT)?
    } else {
        read_u64(info, BYTES_ACKED_AT)?
    };
    Ok(Some(TcpFlow {
        inode,
        received_bytes,
        transmitted_bytes,
    }))
}

/// The `INET_DIAG_INFO` payload, if this message carries one
fn tcp_info(msg: &[u8]) -> Result<Option<&[u8]>, ParseError> {
    let mut at = NLMSG_HDRLEN + DIAG_MSG_LEN;
    while at + 4 <= msg.len() {
        let nla_len = read_u16(msg, at)? as usize;
        let nla_type = read_u16(msg, at + 2)? & NLA_TYPE_MASK;
        if nla_len < 4 || at + nla_len > msg.len() {
            return Err(malformed("attribute overruns the message"));
        }
        if nla_type == INET_DIAG_INFO {
            return Ok(Some(&msg[at + 4..at + nla_len]));
        }
        let step = align4(nla_len);
        if step == 0 {
            return Err(malformed("attribute has no length"));
        }
        at += step;
    }
    Ok(None)
}

fn malformed(message: impl Into<String>) -> ParseError {
    ParseError::Malformed {
        message: message.into(),
    }
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn put_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_ne_bytes());
}

fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

fn read_u16(buf: &[u8], at: usize) -> Result<u16, ParseError> {
    let bytes: [u8; 2] = buf
        .get(at..at + 2)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| malformed("short netlink field"))?;
    Ok(u16::from_ne_bytes(bytes))
}

fn read_u32(buf: &[u8], at: usize) -> Result<u32, ParseError> {
    let bytes: [u8; 4] = buf
        .get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| malformed("short netlink field"))?;
    Ok(u32::from_ne_bytes(bytes))
}

fn read_i32(buf: &[u8], at: usize) -> Result<i32, ParseError> {
    Ok(read_u32(buf, at)? as i32)
}

fn read_u64(buf: &[u8], at: usize) -> Result<u64, ParseError> {
    let bytes: [u8; 8] = buf
        .get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| malformed("short netlink field"))?;
    Ok(u64::from_ne_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u64(buf: &mut [u8], at: usize, value: u64) {
        buf[at..at + 8].copy_from_slice(&value.to_ne_bytes());
    }

    /// A `tcp_info` long enough for `bytes_sent`, with the three counters
    /// planted at the offsets the parser reads
    fn tcp_info(acked: u64, received: u64, sent: u64) -> Vec<u8> {
        let mut info = vec![0u8; MIN_SENT];
        put_u64(&mut info, BYTES_ACKED_AT, acked);
        put_u64(&mut info, BYTES_RECEIVED_AT, received);
        put_u64(&mut info, BYTES_SENT_AT, sent);
        info
    }

    fn attr(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 4 + payload.len()];
        let len = out.len() as u16;
        put_u16(&mut out, 0, len);
        put_u16(&mut out, 2, kind);
        out[4..].copy_from_slice(payload);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out
    }

    fn diag_msg(inode: u32) -> Vec<u8> {
        let mut msg = vec![0u8; DIAG_MSG_LEN];
        put_u32(&mut msg, INODE_AT, inode);
        msg
    }

    /// One `SOCK_DIAG_BY_FAMILY` message: header, diag msg, attributes
    fn socket_msg(seq: u32, inode: u32, attributes: &[u8]) -> Vec<u8> {
        let body = [diag_msg(inode), attributes.to_vec()].concat();
        let len = NLMSG_HDRLEN + body.len();
        let mut msg = vec![0u8; align4(len)];
        put_u32(&mut msg, 0, len as u32);
        put_u16(&mut msg, 4, SOCK_DIAG_BY_FAMILY);
        put_u32(&mut msg, 8, seq);
        msg[NLMSG_HDRLEN..NLMSG_HDRLEN + body.len()].copy_from_slice(&body);
        msg
    }

    fn done_msg(seq: u32, interrupted: bool) -> Vec<u8> {
        let mut msg = vec![0u8; NLMSG_HDRLEN];
        put_u32(&mut msg, 0, NLMSG_HDRLEN as u32);
        put_u16(&mut msg, 4, NLMSG_DONE);
        if interrupted {
            put_u16(&mut msg, 6, NLM_F_DUMP_INTR);
        }
        put_u32(&mut msg, 8, seq);
        msg
    }

    #[test]
    fn the_request_asks_for_every_tcp_socket_and_its_info() {
        let req = request(2, 7);
        assert_eq!(req.len(), 72);
        assert_eq!(u32::from_ne_bytes(req[0..4].try_into().unwrap()), 72);
        assert_eq!(
            u16::from_ne_bytes(req[4..6].try_into().unwrap()),
            SOCK_DIAG_BY_FAMILY
        );
        assert_eq!(
            u16::from_ne_bytes(req[6..8].try_into().unwrap()),
            NLM_F_REQUEST | NLM_F_DUMP
        );
        assert_eq!(u32::from_ne_bytes(req[8..12].try_into().unwrap()), 7);
        assert_eq!(req[16], 2, "family");
        assert_eq!(req[17], IPPROTO_TCP);
        assert_eq!(req[18], EXT_INFO);
        assert_eq!(
            u32::from_ne_bytes(req[20..24].try_into().unwrap()),
            u32::MAX
        );
        assert_eq!(
            u32::from_ne_bytes(req[64..68].try_into().unwrap()),
            u32::MAX
        );
        assert_eq!(
            u32::from_ne_bytes(req[68..72].try_into().unwrap()),
            u32::MAX
        );
    }

    #[test]
    fn byte_counters_prefer_bytes_sent_and_skip_padding() {
        let info = tcp_info(5, 1000, 4000);
        // A padding attribute, then tcp_info, then an attribute we do not read
        let attrs = [
            attr(14, &[]),
            attr(INET_DIAG_INFO, &info),
            attr(4, b"cubic"),
        ]
        .concat();
        let buf = [
            socket_msg(1, 23045, &attrs),
            socket_msg(1, 23046, &attr(INET_DIAG_INFO, &tcp_info(1, 8, 9))),
            done_msg(1, false),
        ]
        .concat();
        let part = parse(1, &buf).unwrap();
        assert!(part.done);
        assert!(!part.interrupted);
        assert_eq!(part.errno, None);
        assert_eq!(
            part.flows,
            vec![
                TcpFlow {
                    inode: 23045,
                    received_bytes: 1000,
                    transmitted_bytes: 4000,
                },
                TcpFlow {
                    inode: 23046,
                    received_bytes: 8,
                    transmitted_bytes: 9,
                },
            ]
        );
    }

    #[test]
    fn an_older_tcp_info_reports_bytes_acked() {
        let mut info = tcp_info(40, 1000, 9999);
        info.truncate(MIN_RECEIVED);
        let buf = [
            socket_msg(3, 11, &attr(INET_DIAG_INFO, &info)),
            done_msg(3, false),
        ]
        .concat();
        let flow = parse(3, &buf).unwrap().flows.pop().unwrap();
        assert_eq!(flow.received_bytes, 1000);
        assert_eq!(flow.transmitted_bytes, 40, "bytes_acked stands in");
    }

    #[test]
    fn time_wait_with_no_inode_is_skipped() {
        let buf = [
            socket_msg(1, 0, &[]),
            socket_msg(1, 9, &attr(INET_DIAG_INFO, &tcp_info(0, 1, 2))),
            done_msg(1, true),
        ]
        .concat();
        let part = parse(1, &buf).unwrap();
        assert!(part.interrupted);
        assert_eq!(part.flows.len(), 1);
        assert_eq!(part.flows[0].inode, 9);
    }

    #[test]
    fn a_socket_without_tcp_info_is_refused() {
        let buf = [socket_msg(1, 9, &[]), done_msg(1, false)].concat();
        let err = parse(1, &buf).unwrap_err();
        assert!(
            matches!(err, ParseError::Malformed { ref message } if message.contains("missing its tcp_info")),
            "{err:?}"
        );
    }

    #[test]
    fn a_tcp_info_without_byte_counters_is_too_old() {
        let buf = [
            socket_msg(1, 9, &attr(INET_DIAG_INFO, &[0u8; 32])),
            done_msg(1, false),
        ]
        .concat();
        assert!(matches!(
            parse(1, &buf).unwrap_err(),
            ParseError::NoByteCounters { bytes: 32 }
        ));
    }

    #[test]
    fn a_kernel_refusal_is_the_errno() {
        let mut msg = vec![0u8; NLMSG_HDRLEN + 4];
        let len = msg.len() as u32;
        put_u32(&mut msg, 0, len);
        put_u16(&mut msg, 4, NLMSG_ERROR);
        put_u32(&mut msg, 8, 2);
        put_u32(&mut msg, NLMSG_HDRLEN, (-97i32) as u32);
        let part = parse(2, &msg).unwrap();
        assert_eq!(part.errno, Some(-97));
        assert!(!part.done);
    }

    #[test]
    fn a_truncated_message_is_refused() {
        let mut buf = socket_msg(1, 9, &attr(INET_DIAG_INFO, &tcp_info(0, 1, 2)));
        buf.pop();
        assert!(matches!(parse(1, &buf), Err(ParseError::Malformed { .. })));
        assert!(matches!(parse(1, &[]), Err(ParseError::Malformed { .. })));
    }

    #[test]
    fn a_stray_sequence_is_refused() {
        let buf = done_msg(9, false);
        let err = parse(1, &buf).unwrap_err();
        assert!(
            matches!(err, ParseError::Malformed { ref message } if message.contains("sequence")),
            "{err:?}"
        );
    }
}
