use std::io::{self, IsTerminal, Write};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};

use crate::{failure, HostResult};

#[derive(Default)]
pub(crate) struct Terminal {
    active: bool,
    raw_enabled: bool,
    #[cfg(unix)]
    pipe: Option<PipeInput>,
}

impl Terminal {
    pub fn enter(&mut self) -> HostResult<()> {
        if !self.active {
            if io::stdin().is_terminal() {
                // Leave raw mode alone when it belongs to the invoking host.
                if !is_raw_mode_enabled()? {
                    enable_raw_mode()?;
                    self.raw_enabled = true;
                }
            } else {
                #[cfg(unix)]
                {
                    self.pipe = Some(PipeInput::open()?);
                }
                #[cfg(not(unix))]
                return Err(failure("non-TTY terminal input is supported only on Unix"));
            }
            self.active = true;
        }
        Ok(())
    }

    pub fn restore(&mut self) -> HostResult<()> {
        if self.active {
            // Do not mark restored until raw mode was actually disabled: Drop
            // must retry if this operation fails while unwinding JS effects.
            if self.raw_enabled {
                disable_raw_mode()?;
                self.raw_enabled = false;
            }
            #[cfg(unix)]
            if let Some(pipe) = &mut self.pipe {
                pipe.restore()?;
                self.pipe = None;
            }
            let mut output = io::stdout().lock();
            output.write_all(b"\x1b[0m\x1b[?25h\x1b[?7h")?;
            output.flush()?;
            self.active = false;
        }
        Ok(())
    }

    pub fn key(&mut self) -> HostResult<String> {
        if !self.active {
            return Err(failure("terminal input has not been entered"));
        }
        #[cfg(unix)]
        if let Some(pipe) = &mut self.pipe {
            return pipe.key().map_err(Into::into);
        }
        // Return one protocol token, not a batch of unrelated keystrokes.
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                let token = match key.code {
                    KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let c = c.to_ascii_lowercase();
                        if c.is_ascii_lowercase() {
                            char::from((c as u8 - b'a') + 1).to_string()
                        } else {
                            c.to_string()
                        }
                    }
                    KeyCode::Char(c) => c.to_string(),
                    KeyCode::Esc => "\x1b".into(),
                    KeyCode::Up => "\x1b[A".into(),
                    KeyCode::Down => "\x1b[B".into(),
                    KeyCode::Right => "\x1b[C".into(),
                    KeyCode::Left => "\x1b[D".into(),
                    KeyCode::Enter => "\r".into(),
                    KeyCode::Tab => "\t".into(),
                    KeyCode::Backspace => "\x7f".into(),
                    KeyCode::Home => "\x1b[H".into(),
                    KeyCode::End => "\x1b[F".into(),
                    KeyCode::Delete => "\x1b[3~".into(),
                    KeyCode::Insert => "\x1b[2~".into(),
                    KeyCode::PageUp => "\x1b[5~".into(),
                    KeyCode::PageDown => "\x1b[6~".into(),
                    _ => continue,
                };
                return Ok(token);
            }
        }
        Ok(String::new())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
struct PipeInput {
    file: std::fs::File,
    original_flags: Option<i32>,
    bytes: Vec<u8>,
    offset: usize,
    eof: bool,
    escape_since: Option<Instant>,
}

#[cfg(unix)]
impl PipeInput {
    fn open() -> io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        // A duplicate keeps stdin open for this input session. File-status
        // flags are shared with stdin, so restore our change before closing it.
        let fd = unsafe { libc::fcntl(io::stdin().as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let original_flags = if flags & libc::O_NONBLOCK == 0 {
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Some(flags)
        } else {
            None
        };
        Ok(Self {
            file,
            original_flags,
            bytes: Vec::new(),
            offset: 0,
            eof: false,
            escape_since: None,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        if let Some(flags) = self.original_flags {
            if unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_SETFL, flags) } < 0 {
                return Err(io::Error::last_os_error());
            }
            self.original_flags = None;
        }
        Ok(())
    }

    fn key(&mut self) -> io::Result<String> {
        use std::io::Read;
        if let Some(token) = self.token(false) {
            return Ok(token);
        }
        if !self.eof {
            let mut chunk = [0; 1024];
            match self.file.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(count) => {
                    if self.offset != 0 {
                        self.bytes.drain(..self.offset);
                        self.offset = 0;
                    }
                    self.bytes.extend_from_slice(&chunk[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(self.token(true).unwrap_or_default())
    }

    fn token(&mut self, allow_escape_timeout: bool) -> Option<String> {
        let bytes = &self.bytes[self.offset..];
        let first = *bytes.first()?;
        let (count, token) = if first == 0x1b {
            // An escape may arrive separately from the rest of an ANSI key.
            // A lone escape must still become available while a pipe is open.
            let since = self.escape_since.get_or_insert_with(Instant::now);
            let complete = if matches!(bytes.get(1), Some(b'[' | b'O')) {
                bytes[2..]
                    .iter()
                    .position(|byte| !(0x20..=0x3f).contains(byte))
                    .filter(|index| (0x40..=0x7e).contains(&bytes[index + 2]))
                    .map(|index| index + 3)
            } else {
                None
            };
            if let Some(count) = complete {
                let sequence = &bytes[..count];
                let canonical = match sequence[count - 1] {
                    b'A' => "\x1b[A",
                    b'B' => "\x1b[B",
                    b'C' => "\x1b[C",
                    b'D' => "\x1b[D",
                    b'H' => "\x1b[H",
                    b'F' => "\x1b[F",
                    _ if matches!(sequence, b"\x1b[1~" | b"\x1b[7~") => "\x1b[H",
                    _ if matches!(sequence, b"\x1b[4~" | b"\x1b[8~") => "\x1b[F",
                    _ => "",
                };
                (
                    count,
                    if canonical.is_empty() {
                        String::from_utf8_lossy(sequence).into_owned()
                    } else {
                        canonical.to_owned()
                    },
                )
            } else {
                let incomplete = bytes.len() == 1
                    || (matches!(bytes.get(1), Some(b'[' | b'O'))
                        && bytes[2..].iter().all(|byte| (0x20..=0x3f).contains(byte)));
                if incomplete
                    && !self.eof
                    && bytes.len() < 64
                    && (!allow_escape_timeout || since.elapsed() < Duration::from_millis(25))
                {
                    return None;
                }
                (1, "\x1b".to_owned())
            }
        } else {
            let width = match first {
                0x00..=0x7f => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => 1,
            };
            let available = &bytes[..width.min(bytes.len())];
            match std::str::from_utf8(available) {
                Ok("\n") => (1, "\r".to_owned()),
                Ok("\x08") => (1, "\x7f".to_owned()),
                Ok(text) => (width, text.to_owned()),
                Err(error) if error.error_len().is_none() && !self.eof => return None,
                Err(error) => (
                    error.error_len().unwrap_or(available.len()),
                    "\u{fffd}".to_owned(),
                ),
            }
        };
        self.offset += count;
        self.escape_since = None;
        if self.offset == self.bytes.len() {
            self.bytes.clear();
            self.offset = 0;
        }
        Some(token)
    }
}

#[cfg(unix)]
impl Drop for PipeInput {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
