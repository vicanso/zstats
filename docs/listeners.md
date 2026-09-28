# Listening sockets

**Status: implemented** as `zstats::listeners()` (`src/listeners.rs`) on
macOS, Linux and Windows. This page started as the proposal; it now records the
question, what was measured, the shape chosen, and two findings from the
implementation that overturned parts of the proposal — the kernel narrows
the macOS table for some callers (§3.1), and the per-process fallback cannot
be written without `unsafe` (§3.3).

The question came from [zstats.app](https://github.com/vicanso/zstats.app):
its Network tab shows interfaces and their rates, and the next thing a
reader asks there is *which program is listening on which port*.

## 1. What is being asked

For every socket that is waiting for someone else to connect:

- the protocol (TCP in `LISTEN`; UDP bound with no remote address);
- the local address and port;
- the owning process (pid and name) and user, where the platform allows.

The address matters as much as the port. `*:6379` and `127.0.0.1:6379` are
the same program on the same port, but only the first one can be reached
from another machine. **Exposure is the headline**, not the port number.

Out of scope: established connections, per-connection byte counts, and any
alert. Per-connection IO needs the private APIs §7 of `metrics.md` already
rejects for per-process network IO. An alert ("a new listener on all
interfaces") would belong in the rule engine and is not proposed here.

## 2. Shape: a one-shot function, not a snapshot channel

The only caller in view wants the list **while a person is looking at it**
(zstats.app queries on Network-tab entry, refreshes while the tab stays on
screen, and stops when it is hidden). That decides the shape:

- **No baseline.** Unlike disk, network or drive rates, a listener list is a
  state, not a diff. Nothing has to be remembered between calls, so it does
  not need to live inside `LocalCollector` or ride `Monitor::tick`.
- **It changes rarely.** Sampling it every collect for the daemon's history
  would pay for a figure nobody records.
- **The daemon's cost stays exactly what it is.** A function nobody calls
  costs nothing; a `SystemSnapshot` field would add a toggle, a cadence and
  a default to argue about.

So: a free function beside the collector, the same class as a one-shot
full process listing.

```rust
/// Every listening socket this platform lets an unprivileged process see.
/// Blocking; call it off the UI thread.
pub fn listeners() -> Result<Listeners, CollectError>;

pub struct Listeners {
    pub sockets: Vec<ListenerSnapshot>,
    /// Whether `pid` / `process` could be filled for every socket, or only
    /// for the caller's own processes (see §4).
    pub coverage: OwnerCoverage,
}

pub struct ListenerSnapshot {
    pub protocol: Protocol,        // Tcp | Udp
    pub address: std::net::IpAddr, // 0.0.0.0 / :: = every interface
    pub port: u16,
    pub pid: Option<u32>,
    pub process: Option<String>,
    /// Seconds the owner has been running, as of the call, from the one
    /// process entry read per pid for the name too. `None` when the OS
    /// will not give the start time (another user's process on macOS) —
    /// sysinfo's 0 there would read as "started just now".
    pub run_time_secs: Option<u64>,
    pub uid: Option<u32>,
}

pub enum OwnerCoverage { AllProcesses, OwnProcessesOnly }
```

`Capabilities` gained `listeners: bool` so a frontend can tell "this
platform cannot" from "nothing is listening", the same way it does for
`gpu` and `drive_io`. v4/v6 twins of one socket (`*:7777` over both
families) are reported separately; merging them is presentation. A
dual-stack socket — one IPv6 socket that also accepts IPv4, `netstat`'s
`tcp46` — is one socket and reports its IPv6 address, `::`. Exact duplicates
(one process holding several `SO_REUSEPORT` sockets on the same address and
port) are collapsed, since the shape has nothing to tell them apart by. The
list is sorted by protocol, port, address, pid.

Errors: `CollectError::Unsupported` off macOS, Linux and Windows,
`CollectError::Restricted` when macOS narrows the view to the caller (§3.1),
`CollectError::System` for a table that cannot be read or does not parse.

A CLI subcommand (`zstats listeners`) was proposed here as the natural place
to eyeball the parser against `netstat`. **It was not added**: the `zstats`
binary is a bare executable, so on macOS it would only ever print the
`Restricted` error (§3.1). The comparison against `netstat` was done by hand
instead, from a process that sees the whole table (§3.2).

## 3. macOS: how to see every owner without root

Measured 2026-09-27 on an M4 Pro, macOS 27.0 (26A428), unprivileged, with
calls spaced 1.5 s apart (back-to-back runs under-report on Apple Silicon;
see §3.12 of `metrics.md`):

| Approach | Listeners with an owner | CPU per call |
|---|---|---|
| Read the kernel's socket table, `net.inet.tcp.pcblist_n` / `udp.pcblist_n` (what `netstat` reads) | **all of them** — 40 TCP (port, pid) pairs, identical to `netstat -anv` | not measured in-process; no child process, one `sysctl` per protocol plus a linear parse (the TCP table read 87–197 KB across reads — it grows with open sockets, 319 at the largest; UDP 28 KB) |
| Spawn `netstat -anv -p tcp` / `-p udp` | all of them | 9 ms / 7 ms |
| Walk each process's file descriptors (`lsof`, or libproc `listpidinfo::<ListFDs>` + `pidfdinfo::<SocketFDInfo>`) | **own processes only** | 45 ms for `lsof` |

The per-process walk is the clean API and the wrong answer: `proc_pidinfo`
on another user's process fails with `EPERM`, so it missed `mDNSResponder`
(`*:53`) and `launchd` (`127.0.0.1:8021`) — the system listeners, which are
exactly the ones "what is exposed on this machine" has to include.

The "all of them" in the first row holds only for some callers — see §3.1.

### 3.1 The kernel narrows the table for bare executables

Found while implementing, macOS 27.0 (26A428). The proposal's measurement was
taken from processes that happen to be exempt; a freshly built Rust binary is
not. Every row below reads `net.inet.tcp.pcblist_n` the same way:

| Caller | What the table holds |
|---|---|
| `/usr/sbin/sysctl` or `netstat`, run from a shell | every socket (≈180 KB) |
| Homebrew `python3` via `ctypes` — which runs as `Python.app` | every socket, 45 owners |
| A bare Rust executable (cargo's default, linker-signed) | **its own sockets only** — 48 bytes when it has none |
| The same executable re-signed ad hoc | its own sockets only |
| The same executable inside a minimal `.app` (ad-hoc signed), run directly or via `open` | every socket |
| The same executable with an Info.plist embedded (`-sectcreate __TEXT __info_plist`) **and** re-signed so the code directory binds it | every socket |
| …embedded Info.plist, linker signature only (`Info.plist=not bound`) | its own sockets only |
| `sysctl` / `netstat` spawned *by* a restricted process | own sockets only — the child inherits it |

"Own" is strict: a child's sockets are not included. So the deciding property
is an app-bundle identity bound into the code signature, and it is the one
consumer in view — zstats.app, a signed bundle — that has it.

Two consequences shaped the implementation:

- **A narrowed table must not be returned as an answer.** It parses cleanly
  and reads as "nothing else is listening". The opening `xinpgen` header's
  `xig_count` still counts every socket (287 while the body held one), so the
  parser reports the restriction precisely: the body holds fewer sockets than
  the header counts **and** every one it holds is the caller's. A full read
  can also come up short when sockets close mid-walk, but it still holds
  system daemons' sockets, so the second half tells the two apart.
  `listeners()` then returns `CollectError::Restricted`.
- **`cargo test` binaries are bare**, so the live test sees only its own
  sockets. That is enough for what it checks — the offsets, against sockets
  it binds — and it calls the internal reader that skips the restriction
  check. A second test asserts that whichever view the test binary gets,
  `listeners()` answers it honestly. A test comparing against `netstat`
  cannot run there at all: `netstat` spawned by the test is restricted too.

Making the `zstats` CLI see the whole table is possible — embed an Info.plist
at link time and codesign after the build — but that changes the binary's
identity to the system and adds a signing step to every install path.
**Decided against** (user decision): the CLI stays a plain binary, and the
listener view belongs to zstats.app.

### 3.2 Verification

From an `.app`-wrapped probe (full view), against `netstat -anv` run from a
shell: 55 listening sockets (43 TCP, 12 UDP), identical `(protocol, port,
pid)` sets in both directions, including `mDNSResponder` (`*:53`, uid 65) and
`launchd` (`127.0.0.1:8021`, uid 0), every one with a process name.

### 3.3 Parsing `pcblist_n` with the `sysctl` crate

Both dependencies are already here; no new crate and no `unsafe`. The
`sysctl` crate returns the table as `CtlValue::Struct(Vec<u8>)`, readable
without root. Its framing, confirmed by walking a live table end to end:

- It opens and closes with an `xinpgen` header (24 bytes; first `u32` is
  its own length). The closing one is how the walk knows it is done.
- In between, every socket is a run of records, each starting with
  `u32 len, u32 kind`. For TCP the run is, in order: `0x10` `xinpcb_n`
  (104 bytes), `0x01` `xsocket_n` (104), `0x02` / `0x04` receive and send
  `xsockbuf_n` (32 each), `0x08` `xsockstat_n` (136), `0x20` `xtcpcb_n`
  (204). A socket starts at each `0x10`.
- **Records are 8-byte aligned.** The stride is `len` rounded up to 8.
  Stepping by the raw `len` falls out of step after the first socket — the
  first attempt at this walk stopped after six records.

The fields a listener needs, located by matching a known listener
(`sccache`, `127.0.0.1:4226`, pid 15482) and then checked against every
LISTEN socket `netstat -anv` printed:

| Field | Record | Offset | Encoding |
|---|---|---|---|
| local port (`inp_lport`) | `0x10` `xinpcb_n` | 18 | `u16`, network byte order |
| owning pid (the pid `netstat -v` prints as `process:pid`) | `0x01` `xsocket_n` | 68 | `u32`, little-endian |
| TCP state (`t_state`, `LISTEN` = 1) | `0x20` `xtcpcb_n` | 36 | `i32`, little-endian |

The rest, located the same way — bind known sockets (`127.0.0.1`, `::1`,
`0.0.0.0`, `::` with and without `IPV6_V6ONLY`, bound and connected UDP) and
find their bytes — and consistent with the XNU headers laid out under
`#pragma pack(4)`:

| Field | Record | Offset | Encoding |
|---|---|---|---|
| foreign port (`inp_fport`) | `0x10` `xinpcb_n` | 16 | `u16`, network byte order |
| `inp_vflag` | `0x10` `xinpcb_n` | 44 | `u8`: `0x1` IPv4, `0x2` IPv6, both = dual-stack |
| foreign address (`inp_dependfaddr`) | `0x10` `xinpcb_n` | 48 | 16 bytes; IPv4 in the last 4 (`in_addr_4in6`) |
| local address (`inp_dependladdr`) | `0x10` `xinpcb_n` | 64 | 16 bytes; IPv4 in the last 4 |
| protocol (`xso_protocol`) | `0x01` `xsocket_n` | 36 | `i32`: 6 / 17 |
| uid (`so_uid`) | `0x01` `xsocket_n` | 64 | `u32` |

UDP counts as listening when the local port is set and the foreign port and
address are empty — the same rule `netstat` uses for `*.*`.

`xso_family` (offset 40) is **not** usable as a check: live tables carry
established and TIME_WAIT sockets whose family reads 0 while every other field
is populated (4 of 288 on the measured machine).

**These layouts are XNU `PRIVATE` structures** (`bsd/netinet/in_pcb.h`,
`bsd/sys/socketvar.h`). `netstat` ships with the OS and moves with it; this
parser does not. Four things make that acceptable:

1. **Validate before trusting.** Every known record's `len` must match the
   size the parser was written against for its `kind`, every socket must
   carry its `xsocket_n` (and, for TCP, its `xtcpcb_n`), and every attached
   socket's protocol must be the table's. Any mismatch means the layout
   moved, and the whole table is refused rather than half-read. Two things
   are deliberately tolerated: record kinds the parser does not know (framed
   by their length and stepped over — a new kind moves nothing it reads),
   and a PCB with no socket attached (an `xsocket_n` left all zero), which
   can be neither an owner nor a listener.
2. **Refuse, never guess** — but not by falling back. The proposed fallback,
   the libproc per-process walk, **cannot be written in this crate**: the
   per-protocol half of libproc's `SocketInfo` is a Rust `union`, and reading
   a union field is `unsafe`, which `#![forbid(unsafe_code)]` rules out.
   Spawning `netstat` instead would be no fallback either — it reads the same
   table and inherits the same restriction (§3.1). A refused table is
   therefore a `CollectError::System` naming the record that did not match; a
   macOS update that moves the layout costs the feature until the parser is
   updated, never a wrong list.
3. **A fixture test** over a synthetic table (framing, the 8-byte stride,
   every field above, each refusal rule, restriction detection). Synthetic
   rather than captured: a captured table carries the machine's live
   connections, remote addresses included, and would publish them in the
   repository.
4. **A live test** binds TCP on `127.0.0.1`, `0.0.0.0` and `::1` plus a UDP
   socket, and asserts each comes back with this test's own pid, uid and
   address, while a connected UDP socket and an accepted TCP stream do not
   come back at all — so a layout change on CI's macOS fails loudly.

### 3.4 The alternative, and why not

Spawning `netstat` is the most robust against layout changes — its text is
built by Apple against the matching headers. It is rejected as the default
because it would be the library's **second** child process (`ioreg` is
documented as the one), because parsing a column-aligned text table trades
one fragility for another, and because 16 ms a refresh is paid on every
visit where the in-process read pays for one `sysctl`. It remains the
reference to compare against — by hand, since a `netstat` spawned from a
test binary is restricted (§3.1).

### 3.5 Measured cost

In-process, from the full view, calls spaced 1.5 s apart: **0.9–4.4 ms of CPU
per call** (median ≈ 3.5 ms), no child process, both tables plus owner names.
That is the whole call — a frontend refreshing every 15 s while a tab is on
screen spends about 0.02% of a core.

## 4. Linux and Windows

- **Linux — implemented.** `/proc/net/{tcp,tcp6,udp,udp6}` list every
  socket with its local address, state, uid and inode, readable by any user.
  Mapping an inode to a pid means reading `/proc/<pid>/fd/*` links, which
  the kernel allows only for the caller's own processes without extra
  privileges — so ports, addresses and users are complete. Coverage is
  reported from what actually happened: `OwnProcessesOnly` when some
  `/proc/<pid>/fd` was refused *and* a wanted inode stayed unowned, otherwise
  `AllProcesses` (root, or every listener happened to be ours). A socket held
  by several processes goes to the lowest pid, normally a pre-fork server's
  parent. Plain file reads; no dependency. Covered by fixture tests on every
  platform and compiled by `make check-targets`, but not run on a Linux
  machine here.
- **Windows — implemented through `netstat2`** (user decision; a
  Windows-target dependency, MIT/Apache-2.0, which pulls only `bitflags` and
  `thiserror` there and keeps the FFI out of this crate).
  `GetExtendedTcpTable` / `GetExtendedUdpTable` with the `*_OWNER_PID_*`
  table classes return a pid for every socket without elevation, so coverage
  is `AllProcesses`; names come from the same targeted sysinfo lookup as the
  other platforms. Two differences are inherent to the API: the UDP table
  has no remote end, so a connected UDP socket cannot be told apart from a
  listening one and every bound UDP endpoint is reported; and there are no
  uids (`uid: None`). Pid 0, the System Idle Process, is reported as no owner;
  pid 4 (System) is a real one. The row mapping is a platform-neutral
  function tested on every platform, and the crate's Windows build is
  linted by `cargo clippy --target x86_64-pc-windows-msvc`, but it has not
  run on a Windows machine here.

## 5. How a frontend should show it

- **Group by process, lead with exposure.** "All interfaces" (`0.0.0.0`,
  `::`) versus "this machine only" (`127.0.0.1`, `::1`) versus a specific
  address is the fact worth colouring; the port list follows.
- **Merge v4/v6 twins** of one port and process into one row.
- **Say when owners are partial.** Under `OwnProcessesOnly`, a row with no
  process is "another user's", not "unknown".
- **Treat `Restricted` as "open me from the app"**, not as an empty machine.
  zstats.app never sees it; an embedder shipping a bare binary always will
  on macOS.
- **Query on sight, not on a schedule.** zstats.app's plan: call on
  Network-tab entry on a background thread, refresh every 15 s while the tab
  is on screen (its network cadence), drop the result when the panel hides.

## 6. Decisions

- **`pcblist_n`, parsed in-process** — every owner for a bundled caller, no
  child process, ≈3.5 ms. `netstat` would not have been the sturdier choice
  it looked like: it reads the same table under the same restriction.
- **No fallback**; a table that does not validate is an error (§3, point 2).
- **A narrowed view is an error**, not a short list (§3.1).
- **No CLI subcommand and no Info.plist in the CLI binary** (user decision):
  the listener view is zstats.app's (§3.1).
- **Windows through `netstat2`** (user decision, §4).
