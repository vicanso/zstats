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

//! Interactive key handling for watch / attach live views (unix only).

/// How the user left a live foreground view
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LiveExit {
    /// Quit the foreground view (and, in watch mode, the whole process)
    Quit,
    /// Leave the foreground view while keeping the collector daemon running
    Detach,
}

/// Put stdin into raw-ish mode (no line buffering, no echo) so single
/// keypresses arrive immediately. ISIG stays on, so Ctrl+C still works.
/// Restores the original settings on drop.
#[cfg(unix)]
pub struct RawMode {
    original: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    /// Enable raw-ish mode when stdin is a TTY. Returns None otherwise.
    pub fn enable() -> Option<Self> {
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) == 0 {
                return None;
            }
            let mut term = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return None;
            }
            let original = term;
            term.c_lflag &= !(libc::ICANON | libc::ECHO);
            term.c_cc[libc::VMIN] = 1;
            term.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) != 0 {
                return None;
            }
            Some(Self { original })
        }
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}

/// Wait for the user to end a live view: Ctrl+C always quits; on an
/// interactive terminal (raw stdin) `q` quits and `d` detaches.
///
/// `interactive` is typically "stdout is a TTY" (live screen is active).
/// Raw mode still requires stdin to be a TTY — when that fails, only
/// Ctrl+C is accepted.
#[cfg(unix)]
pub async fn wait_live_exit(interactive: bool) -> LiveExit {
    use tokio::io::AsyncReadExt as _;

    let raw = if interactive { RawMode::enable() } else { None };
    if raw.is_none() {
        let _ = tokio::signal::ctrl_c().await;
        return LiveExit::Quit;
    }
    let _raw = raw;

    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 1];
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return LiveExit::Quit,
            result = stdin.read(&mut buf) => match result {
                Ok(n) if n > 0 => match buf[0] {
                    b'q' | b'Q' => return LiveExit::Quit,
                    b'd' | b'D' => return LiveExit::Detach,
                    _ => {}
                },
                _ => {
                    let _ = tokio::signal::ctrl_c().await;
                    return LiveExit::Quit;
                }
            }
        }
    }
}

#[cfg(not(unix))]
pub async fn wait_live_exit(_interactive: bool) -> LiveExit {
    let _ = tokio::signal::ctrl_c().await;
    LiveExit::Quit
}

/// Map a single key byte (when already in raw mode) to a live-exit action.
/// Returns None for keys that should be ignored.
pub fn key_to_exit(byte: u8) -> Option<LiveExit> {
    match byte {
        b'q' | b'Q' => Some(LiveExit::Quit),
        b'd' | b'D' => Some(LiveExit::Detach),
        _ => None,
    }
}
