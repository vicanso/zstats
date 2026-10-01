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

//! Netlink socket for a `SOCK_DIAG` TCP dump.
//!
//! [`super::sockdiag`] parses the bytes. This module only moves them: one
//! datagram socket, a dump of `AF_INET` and then `AF_INET6`, and the rule
//! for a walk the kernel says was torn. The FFI stays in `nix`, so
//! `forbid(unsafe_code)` still holds.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use nix::errno::Errno;
use nix::sys::socket::sockopt::{RcvBuf, ReceiveTimeout};
use nix::sys::socket::{
    AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, recv, sendto,
    setsockopt, socket,
};
use nix::sys::time::TimeVal;

use super::sockdiag::{self, ParseError, TcpFlow};
use crate::error::CollectError;

/// `AF_INET`. Numeric so this path does not take a dependency on `libc`
const AF_INET: u8 = 2;
/// `AF_INET6`
const AF_INET6: u8 = 10;

/// Userspace copy of one dump datagram. The kernel batches a walk into
/// messages of a few dozen KiB. `nix`'s `recv` cannot report `MSG_TRUNC`,
/// so this stays larger than one datagram: a copy that ran out of room
/// fails the parse as a short message instead of being summed
const RECV_BYTES: usize = 256 * 1024;

/// How many datagrams one family dump may take before it is given up
const RECV_CAP: usize = 4096;

/// How many times a walk marked `NLM_F_DUMP_INTR` is started over
const DUMP_ATTEMPTS: usize = 3;

/// Asked of `SO_RCVBUF`. The kernel clamps it to `rmem_max` and doubles
/// what it stores; the point is that one datagram is not dropped before
/// [`RECV_BYTES`] can copy it
const RCVBUF_BYTES: usize = 1024 * 1024;

/// One second of silence. A dump that never sends `NLMSG_DONE` must not
/// hang the caller
const RECV_TIMEOUT: TimeVal = TimeVal::new(1, 0);

/// Every TCP socket's counters, keyed by inode. The same inode in both
/// families, or twice in a torn walk, keeps the later copy
pub(super) fn tcp_flows() -> Result<Vec<TcpFlow>, CollectError> {
    let fd = open_socket()?;
    let mut buf = vec![0u8; RECV_BYTES];
    let mut by_inode = HashMap::new();
    // Sequence 1 is IPv4 and 2 is IPv6. A reply carrying the other one is
    // refused by the parser rather than mixed into this family
    dump_family(&fd, &mut buf, AF_INET, 1, false, &mut by_inode)?;
    dump_family(&fd, &mut buf, AF_INET6, 2, true, &mut by_inode)?;
    Ok(by_inode.into_values().collect())
}

fn open_socket() -> Result<OwnedFd, CollectError> {
    let fd = socket(
        AddressFamily::Netlink,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::NetlinkSockDiag,
    )
    .map_err(|err| diag_err(format!("socket: {err}")))?;
    // Port id 0 asks the kernel to assign one. `CLOEXEC` so a child
    // spawned while the dump is in flight does not inherit it
    let kernel = NetlinkAddr::new(0, 0);
    bind(fd.as_raw_fd(), &kernel).map_err(|err| diag_err(format!("bind: {err}")))?;
    setsockopt(&fd, RcvBuf, &RCVBUF_BYTES).map_err(|err| diag_err(format!("SO_RCVBUF: {err}")))?;
    setsockopt(&fd, ReceiveTimeout, &RECV_TIMEOUT)
        .map_err(|err| diag_err(format!("SO_RCVTIMEO: {err}")))?;
    Ok(fd)
}

enum FamilyDump {
    /// This address family is not in the kernel. IPv6 only
    Absent,
    Ready {
        flows: Vec<TcpFlow>,
        interrupted: bool,
    },
}

/// Up to [`DUMP_ATTEMPTS`] walks. A parse or IO error is returned as-is:
/// the caller drops the socket, and a retry on a desynced stream would
/// mix two generations. A walk that stays interrupted keeps its last
/// complete `NLMSG_DONE` — the counters on the sockets it did see are
/// still that kernel's totals, and a socket missed this round is the same
/// case as one that closed
fn dump_family(
    fd: &OwnedFd,
    buf: &mut [u8],
    family: u8,
    seq: u32,
    ipv6: bool,
    into: &mut HashMap<u64, TcpFlow>,
) -> Result<(), CollectError> {
    let mut kept = HashMap::new();
    for _ in 0..DUMP_ATTEMPTS {
        match read_dump(fd, buf, family, seq, ipv6)? {
            FamilyDump::Absent => return Ok(()),
            FamilyDump::Ready { flows, interrupted } => {
                kept.clear();
                for flow in flows {
                    kept.insert(flow.inode, flow);
                }
                if !interrupted {
                    into.extend(kept);
                    return Ok(());
                }
                // `NLM_F_DUMP_INTR`, and `NLMSG_DONE` already drained this
                // socket, so the same sequence can be reused
            }
        }
    }
    into.extend(kept);
    Ok(())
}

fn read_dump(
    fd: &OwnedFd,
    buf: &mut [u8],
    family: u8,
    seq: u32,
    ipv6: bool,
) -> Result<FamilyDump, CollectError> {
    send_all(fd.as_raw_fd(), &sockdiag::request(family, seq))?;
    let mut flows = Vec::new();
    let mut recvs = 0usize;
    loop {
        if recvs >= RECV_CAP {
            return Err(diag_err("dump did not finish"));
        }
        let n = match recv(fd.as_raw_fd(), buf, MsgFlags::empty()) {
            Ok(0) => return Err(diag_err("empty reply")),
            Ok(n) => n,
            Err(Errno::EINTR) => continue,
            // `EWOULDBLOCK` is the same value as `EAGAIN` here. Matching
            // both is an unreachable pattern
            Err(Errno::EAGAIN) => return Err(diag_err("dump timed out")),
            Err(err) => return Err(diag_err(format!("recv: {err}"))),
        };
        recvs += 1;
        let part = sockdiag::parse(seq, &buf[..n]).map_err(|err| match err {
            ParseError::NoByteCounters { bytes } => CollectError::Unsupported {
                what: format!("tcp_info is {bytes} bytes and has no tcpi_bytes_received"),
            },
            ParseError::Malformed { message } => diag_err(message),
        })?;
        if let Some(err) = part.errno {
            return family_errno(err, ipv6);
        }
        flows.extend(part.flows);
        if part.done {
            return Ok(FamilyDump::Ready {
                flows,
                interrupted: part.interrupted,
            });
        }
    }
}

/// `NLMSG_ERROR` carries a negative errno. IPv6 disabled is an empty
/// family; the same refusal on IPv4 means the dump itself failed
fn family_errno(err: i32, ipv6: bool) -> Result<FamilyDump, CollectError> {
    let code = if err < 0 { err.saturating_neg() } else { err };
    let errno = Errno::from_raw(code);
    if ipv6 && matches!(errno, Errno::EAFNOSUPPORT | Errno::EPROTONOSUPPORT) {
        return Ok(FamilyDump::Absent);
    }
    // `from_raw` collapses an errno it does not know into `UnknownErrno`,
    // which drops the number. Keep it in the message
    Err(diag_err(format!("{errno} ({code})")))
}

fn send_all(fd: RawFd, mut bytes: &[u8]) -> Result<(), CollectError> {
    let kernel = NetlinkAddr::new(0, 0);
    while !bytes.is_empty() {
        match sendto(fd, bytes, &kernel, MsgFlags::empty()) {
            Ok(0) => return Err(diag_err("send wrote nothing")),
            Ok(n) => bytes = &bytes[n..],
            Err(Errno::EINTR) => continue,
            Err(err) => return Err(diag_err(format!("send: {err}"))),
        }
    }
    Ok(())
}

fn diag_err(message: impl std::fmt::Display) -> CollectError {
    CollectError::System {
        message: format!("sock_diag: {message}"),
    }
}
