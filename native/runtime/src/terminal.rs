use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};

use crate::graphics::Graphics;
use crate::{failure, HostResult};

const GRAPHICS_IMAGE_A: u32 = 2_147_483_647;
const GRAPHICS_IMAGE_B: u32 = 2_147_483_646;
const GRAPHICS_PLACEMENT_ID: u32 = 2_147_483_645;
const GRAPHICS_QUERY_ID: u32 = 2_147_483_644;
const QUERY_WAIT: Duration = Duration::from_millis(300);
const RAW_CHUNK: usize = 3 * 1024;

#[derive(Default)]
pub(crate) struct Terminal {
    active: bool,
    raw_enabled: bool,
    #[cfg(unix)]
    input: Option<TtyInput>,
    #[cfg(unix)]
    pipe: Option<PipeInput>,
    graphics_active: bool,
    displayed_image: Option<u32>,
    png: Vec<u8>,
    encoded: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum Control {
    Kitty {
        parameters: Vec<u8>,
        payload: Vec<u8>,
    },
    Csi(Vec<u8>),
}

enum Input {
    Control(Control),
    Key(String),
}

impl Terminal {
    pub fn enter(&mut self) -> HostResult<()> {
        if self.active {
            return Ok(());
        }
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
        Ok(())
    }

    pub fn restore(&mut self) -> HostResult<()> {
        if !self.active {
            return Ok(());
        }
        let mut result = self.leave_graphics();
        let mut input_restored = true;
        #[cfg(unix)]
        if let Some(input) = &mut self.input {
            let restored = input.restore().map_err(Into::into);
            input_restored = restored.is_ok();
            if input_restored {
                self.input = None;
            }
            if result.is_ok() {
                result = restored;
            }
        }
        #[cfg(unix)]
        if let Some(pipe) = &mut self.pipe {
            let restored = pipe.restore().map_err(Into::into);
            input_restored = input_restored && restored.is_ok();
            if restored.is_ok() {
                self.pipe = None;
            }
            if result.is_ok() {
                result = restored;
            }
        }
        if self.raw_enabled {
            let restored = disable_raw_mode().map_err(Into::into);
            input_restored = input_restored && restored.is_ok();
            if restored.is_ok() {
                self.raw_enabled = false;
            }
            if result.is_ok() {
                result = restored;
            }
        }
        let reset = (|| -> HostResult<()> {
            let mut output = io::stdout().lock();
            output.write_all(b"\x1b[0m\x1b[?25h\x1b[?7h")?;
            output.flush()?;
            Ok(())
        })();
        let reset_ok = reset.is_ok();
        if result.is_ok() {
            result = reset;
        }
        if input_restored && reset_ok && !self.graphics_active {
            self.active = false;
        }
        result
    }

    pub fn key(&mut self) -> HostResult<String> {
        if !self.active {
            return Err(failure("terminal input has not been entered"));
        }
        #[cfg(unix)]
        if self.graphics_active {
            self.graphics_error()?;
        }
        #[cfg(unix)]
        if let Some(input) = &mut self.input {
            return input.key().map_err(Into::into);
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

    pub fn open_graphics(&mut self) -> HostResult<()> {
        #[cfg(not(unix))]
        return Err(failure("terminal graphics is supported only on Unix"));
        #[cfg(unix)]
        {
            if self.graphics_active {
                return Ok(());
            }
            if !io::stdout().is_terminal() {
                return Err(failure("terminal graphics requires stdout to be a TTY"));
            }
            let entered_here = !self.active;
            if let Err(error) = self.enter() {
                return Err(error);
            }
            if self.input.is_none() {
                let input = TtyInput::open().map_err(|error| {
                    failure(format!("could not open controlling terminal: {error}"))
                });
                match input {
                    Ok(input) => self.input = Some(input),
                    Err(error) => {
                        if entered_here {
                            let _ = self.restore();
                        }
                        return Err(error);
                    }
                }
            }
            let probed = self.probe_graphics();
            if let Err(error) = probed {
                if entered_here {
                    let _ = self.restore();
                }
                return Err(error);
            }
            // Mark this before emitting control bytes so every partial
            // alternate-screen acquisition is unwound by the same path.
            self.graphics_active = true;
            let acquired = (|| -> HostResult<()> {
                let mut output = io::stdout().lock();
                output.write_all(b"\x1b[?1049h\x1b[?25l\x1b[?7l")?;
                output.flush()?;
                Ok(())
            })();
            if let Err(error) = acquired {
                let _ = self.leave_graphics();
                if entered_here {
                    let _ = self.restore();
                }
                return Err(error);
            }
            Ok(())
        }
    }

    pub fn graphics_size(&mut self) -> HostResult<(u16, u16, u16, u16)> {
        #[cfg(not(unix))]
        return Err(failure("terminal graphics is supported only on Unix"));
        #[cfg(unix)]
        {
            if !self.graphics_active {
                return Err(failure("terminal graphics has not been opened"));
            }
            let cells = window_size()?;
            let (width, height) = if cells.2 > 0 && cells.3 > 0 {
                (cells.2, cells.3)
            } else {
                self.query_pixel_size()?
            };
            if cells.0 == 0 || cells.1 == 0 || width == 0 || height == 0 {
                return Err(failure(
                    "terminal did not report valid cell and pixel dimensions",
                ));
            }
            Ok((cells.0, cells.1, width, height))
        }
    }

    pub fn present(
        &mut self,
        graphics: &mut Graphics,
        surface: u32,
        column: i32,
        row: i32,
    ) -> HostResult<()> {
        #[cfg(not(unix))]
        return Err(failure("terminal graphics is supported only on Unix"));
        #[cfg(unix)]
        {
            if !self.graphics_active {
                return Err(failure("terminal graphics has not been opened"));
            }
            if column <= 0 || row <= 0 {
                return Err(failure(
                    "terminal graphics positions are 1-based and must be positive",
                ));
            }
            self.graphics_error()?;
            graphics.encode_png(surface, &mut self.png)?;
            if self.png.is_empty() {
                return Err(failure("canvas PNG encoding produced no bytes"));
            }
            let image = match self.displayed_image {
                Some(GRAPHICS_IMAGE_A) => GRAPHICS_IMAGE_B,
                _ => GRAPHICS_IMAGE_A,
            };
            let previous = self.displayed_image;
            let mut output = io::stdout().lock();
            output.write_all(b"\x1b7")?;
            write!(output, "\x1b[{row};{column}H")?;
            let transfer = (|| -> HostResult<()> {
                let chunk_count = self
                    .png
                    .len()
                    .checked_add(RAW_CHUNK - 1)
                    .ok_or_else(|| failure("canvas PNG is too large for terminal transfer"))?
                    / RAW_CHUNK;
                for index in 0..chunk_count {
                    let start = index * RAW_CHUNK;
                    let end = (start + RAW_CHUNK).min(self.png.len());
                    let chunk = &self.png[start..end];
                    let encoded_length = base64_length(chunk.len())?;
                    if self.encoded.len() < encoded_length {
                        self.encoded.resize(encoded_length, 0);
                    }
                    STANDARD
                        .encode_slice(chunk, &mut self.encoded[..encoded_length])
                        .map_err(|error| {
                            failure(format!("could not base64 encode PNG: {error}"))
                        })?;
                    let more = if index + 1 == chunk_count { 0 } else { 1 };
                    if index == 0 {
                        write!(
                            output,
                            "\x1b_Ga=T,f=100,t=d,i={image},p={GRAPHICS_PLACEMENT_ID},q=1,C=1,m={more};"
                        )?;
                    } else {
                        write!(output, "\x1b_Gq=1,m={more};")?;
                    }
                    output.write_all(&self.encoded[..encoded_length])?;
                    output.write_all(b"\x1b\\")?;
                }
                if let Some(previous) = previous {
                    write!(
                        output,
                        "\x1b_Ga=d,d=I,i={previous},p={GRAPHICS_PLACEMENT_ID},q=2;\x1b\\"
                    )?;
                }
                Ok(())
            })();
            let restored = output.write_all(b"\x1b8").and_then(|_| output.flush());
            if let Err(error) = transfer {
                let _ = restored;
                return Err(error);
            }
            restored?;
            self.displayed_image = Some(image);
            Ok(())
        }
    }

    #[cfg(unix)]
    fn graphics_error(&mut self) -> HostResult<()> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| failure("controlling terminal input is unavailable"))?;
        input.drain(false)?;
        if let Some(message) = input.take_session_error() {
            return Err(failure(format!(
                "terminal rejected graphics transfer: {message}"
            )));
        }
        Ok(())
    }

    pub fn close_graphics(&mut self) -> HostResult<()> {
        if !self.graphics_active {
            return Ok(());
        }
        self.restore()
    }

    fn leave_graphics(&mut self) -> HostResult<()> {
        if !self.graphics_active {
            return Ok(());
        }
        let mut output = io::stdout().lock();
        let delete_a = write!(
            output,
            "\x1b_Ga=d,d=I,i={GRAPHICS_IMAGE_A},p={GRAPHICS_PLACEMENT_ID},q=2;\x1b\\"
        );
        let delete_b = write!(
            output,
            "\x1b_Ga=d,d=I,i={GRAPHICS_IMAGE_B},p={GRAPHICS_PLACEMENT_ID},q=2;\x1b\\"
        );
        let leave = output
            .write_all(b"\x1b[?7h\x1b[?25h\x1b[?1049l")
            .and_then(|_| output.flush());
        if leave.is_ok() {
            self.graphics_active = false;
            self.displayed_image = None;
        }
        delete_a.and(delete_b).and(leave).map_err(Into::into)
    }

    #[cfg(unix)]
    fn probe_graphics(&mut self) -> HostResult<()> {
        {
            let mut output = io::stdout().lock();
            write!(
                output,
                "\x1b_Ga=q,i={GRAPHICS_QUERY_ID},s=1,v=1,t=d,f=24;AAAA\x1b\\\x1b[c"
            )?;
            output.flush()?;
        }
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| failure("controlling terminal input is unavailable"))?;
        let deadline = Instant::now() + QUERY_WAIT;
        loop {
            input.drain(false)?;
            if let Some(status) = input.take_kitty_status(GRAPHICS_QUERY_ID) {
                return if status {
                    Ok(())
                } else {
                    Err(failure(
                        "terminal rejected the Kitty graphics capability query",
                    ))
                };
            }
            if input.take_device_attributes() {
                return Err(failure(
                    "terminal does not support the Kitty graphics protocol",
                ));
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    "terminal did not respond to the Kitty graphics protocol query",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    fn query_pixel_size(&mut self) -> HostResult<(u16, u16)> {
        {
            let mut output = io::stdout().lock();
            output.write_all(b"\x1b[14t")?;
            output.flush()?;
        }
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| failure("controlling terminal input is unavailable"))?;
        let deadline = Instant::now() + QUERY_WAIT;
        loop {
            input.drain(false)?;
            if let Some(size) = input.take_pixel_size() {
                return Ok(size);
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    "terminal did not report pixel dimensions through TIOCGWINSZ or CSI 14 t",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn base64_length(input: usize) -> HostResult<usize> {
    input
        .checked_add(2)
        .and_then(|size| size.checked_div(3))
        .and_then(|size| size.checked_mul(4))
        .ok_or_else(|| failure("canvas PNG is too large for base64 transfer"))
}

#[cfg(unix)]
fn window_size() -> HostResult<(u16, u16, u16, u16)> {
    use std::os::fd::AsRawFd;
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    let result = unsafe {
        libc::ioctl(
            io::stdout().as_raw_fd(),
            libc::TIOCGWINSZ,
            size.as_mut_ptr(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let size = unsafe { size.assume_init() };
    Ok((size.ws_col, size.ws_row, size.ws_xpixel, size.ws_ypixel))
}

#[cfg(unix)]
struct TtyInput {
    file: std::fs::File,
    original_flags: i32,
    original_mode: libc::termios,
    bytes: Vec<u8>,
    offset: usize,
    escape_since: Option<Instant>,
    controls: VecDeque<Control>,
    keys: VecDeque<String>,
    restored: bool,
}

#[cfg(unix)]
impl TtyInput {
    fn open() -> io::Result<Self> {
        use std::ffi::CString;
        use std::os::fd::FromRawFd;
        let path = CString::new("/dev/tty").expect("literal terminal path has no NUL");
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if original_flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut original_mode = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original_mode.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let original_mode = unsafe { original_mode.assume_init() };
        let mut raw_mode = original_mode;
        unsafe { libc::cfmakeraw(&mut raw_mode) };
        raw_mode.c_cc[libc::VMIN] = 0;
        raw_mode.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw_mode) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file,
            original_flags,
            original_mode,
            bytes: Vec::new(),
            offset: 0,
            escape_since: None,
            controls: VecDeque::new(),
            keys: VecDeque::new(),
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        use std::os::fd::AsRawFd;
        let fd = self.file.as_raw_fd();
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.original_mode) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFL, self.original_flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.restored = true;
        Ok(())
    }

    fn key(&mut self) -> io::Result<String> {
        self.drain(true)?;
        Ok(self.keys.pop_front().unwrap_or_default())
    }

    fn drain(&mut self, allow_escape_timeout: bool) -> io::Result<()> {
        self.read_available()?;
        while let Some(input) = self.next(allow_escape_timeout) {
            match input {
                // q=1 suppresses successful transfer acknowledgements, but
                // consume them if a terminal sends one anyway.
                Input::Control(control) if session_acknowledgement(&control) => {}
                Input::Control(control) => self.controls.push_back(control),
                Input::Key(key) => self.keys.push_back(key),
            }
        }
        Ok(())
    }

    fn read_available(&mut self) -> io::Result<()> {
        use std::io::Read;
        if self.offset != 0 {
            self.bytes.drain(..self.offset);
            self.offset = 0;
        }
        loop {
            let mut chunk = [0; 1024];
            match self.file.read(&mut chunk) {
                // A non-blocking terminal can report an empty read without
                // reaching EOF; keep the session available for later input.
                Ok(0) => break,
                Ok(count) => self.bytes.extend_from_slice(&chunk[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn next(&mut self, allow_escape_timeout: bool) -> Option<Input> {
        let bytes = &self.bytes[self.offset..];
        let first = *bytes.first()?;
        if bytes == b"\x1b_" {
            return None;
        }
        if first == 0x1b && matches!(bytes.get(1), Some(b'_')) && matches!(bytes.get(2), Some(b'G'))
        {
            let (count, control) = parse_kitty_control(bytes)?;
            self.consume(count);
            return Some(Input::Control(control));
        }
        if first == 0x1b && matches!(bytes.get(1), Some(b'[')) {
            if let Some((count, control)) = parse_csi_control(bytes) {
                self.consume(count);
                return Some(Input::Control(control));
            }
        }
        let (count, token) = key_token(bytes, &mut self.escape_since, allow_escape_timeout, false)?;
        self.consume(count);
        Some(Input::Key(token))
    }

    fn consume(&mut self, count: usize) {
        self.offset += count;
        self.escape_since = None;
        if self.offset == self.bytes.len() {
            self.bytes.clear();
            self.offset = 0;
        }
    }

    fn take_kitty_status(&mut self, id: u32) -> Option<bool> {
        let index = self.controls.iter().position(|control| {
            matches!(
                control,
                Control::Kitty { parameters, .. } if kitty_id(parameters) == Some(id)
            )
        })?;
        match self.controls.remove(index)? {
            Control::Kitty { payload, .. } => Some(payload.as_slice() == b"OK"),
            Control::Csi(_) => None,
        }
    }

    fn take_session_error(&mut self) -> Option<String> {
        let index = self.controls.iter().position(|control| {
            matches!(
                control,
                Control::Kitty {
                    parameters,
                    payload
                } if matches!(
                    kitty_id(parameters),
                    Some(GRAPHICS_IMAGE_A | GRAPHICS_IMAGE_B)
                ) && payload.as_slice() != b"OK"
            )
        })?;
        match self.controls.remove(index)? {
            Control::Kitty { payload, .. } => Some(String::from_utf8_lossy(&payload).into_owned()),
            Control::Csi(_) => None,
        }
    }

    fn take_device_attributes(&mut self) -> bool {
        let index = self.controls.iter().position(
            |control| matches!(control, Control::Csi(sequence) if sequence.last() == Some(&b'c')),
        );
        index
            .and_then(|index| self.controls.remove(index))
            .is_some()
    }

    fn take_pixel_size(&mut self) -> Option<(u16, u16)> {
        let index = self.controls.iter().position(|control| match control {
            Control::Csi(sequence) => pixel_size(sequence).is_some(),
            Control::Kitty { .. } => false,
        })?;
        match self.controls.remove(index)? {
            Control::Csi(sequence) => pixel_size(&sequence),
            Control::Kitty { .. } => None,
        }
    }
}

// Preserve stdin-based key input for the ANSI terminal API. Graphics sessions
// instead use /dev/tty, because stdin may already contain the source document.
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
        if self.offset != 0 {
            self.bytes.drain(..self.offset);
            self.offset = 0;
        }
        if !self.eof {
            let mut chunk = [0; 1024];
            match self.file.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(count) => self.bytes.extend_from_slice(&chunk[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        match key_token(&self.bytes, &mut self.escape_since, true, self.eof) {
            Some((count, token)) => {
                self.offset = count;
                self.escape_since = None;
                Ok(token)
            }
            None => Ok(String::new()),
        }
    }
}

#[cfg(unix)]
impl Drop for PipeInput {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
impl Drop for TtyInput {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn parse_kitty_control(bytes: &[u8]) -> Option<(usize, Control)> {
    if !bytes.starts_with(b"\x1b_G") {
        return None;
    }
    let end = bytes[3..]
        .windows(2)
        .position(|window| window == b"\x1b\\")?
        + 3;
    let body = &bytes[3..end];
    let separator = body.iter().position(|byte| *byte == b';')?;
    let parameters = &body[..separator];
    let payload = &body[separator + 1..];
    Some((
        end + 2,
        Control::Kitty {
            parameters: parameters.to_vec(),
            payload: payload.to_vec(),
        },
    ))
}

fn parse_csi_control(bytes: &[u8]) -> Option<(usize, Control)> {
    if !bytes.starts_with(b"\x1b[") {
        return None;
    }
    let end = bytes[2..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))?
        + 2;
    let sequence = &bytes[..=end];
    if matches!(sequence.last(), Some(b'c' | b't')) {
        Some((end + 1, Control::Csi(sequence.to_vec())))
    } else {
        None
    }
}

fn kitty_id(parameters: &[u8]) -> Option<u32> {
    parameters
        .split(|byte| *byte == b',')
        .find_map(|parameter| parameter.strip_prefix(b"i="))
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse().ok())
}

fn session_acknowledgement(control: &Control) -> bool {
    matches!(
        control,
        Control::Kitty {
            parameters,
            payload
        } if matches!(
            kitty_id(parameters),
            Some(GRAPHICS_IMAGE_A | GRAPHICS_IMAGE_B)
        ) && payload.as_slice() == b"OK"
    )
}

fn pixel_size(sequence: &[u8]) -> Option<(u16, u16)> {
    let body = sequence.strip_prefix(b"\x1b[")?.strip_suffix(b"t")?;
    let mut values = body.split(|byte| *byte == b';');
    if values.next()? != b"4" {
        return None;
    }
    let height = std::str::from_utf8(values.next()?).ok()?.parse().ok()?;
    let width = std::str::from_utf8(values.next()?).ok()?.parse().ok()?;
    if values.next().is_some() || width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

#[cfg(unix)]
fn key_token(
    bytes: &[u8],
    escape_since: &mut Option<Instant>,
    allow_escape_timeout: bool,
    eof: bool,
) -> Option<(usize, String)> {
    let first = *bytes.first()?;
    if first == 0x1b {
        let since = escape_since.get_or_insert_with(Instant::now);
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
            return Some((
                count,
                if canonical.is_empty() {
                    String::from_utf8_lossy(sequence).into_owned()
                } else {
                    canonical.to_owned()
                },
            ));
        }
        let incomplete = bytes.len() == 1
            || (matches!(bytes.get(1), Some(b'[' | b'O'))
                && bytes[2..].iter().all(|byte| (0x20..=0x3f).contains(byte)));
        if incomplete
            && !eof
            && bytes.len() < 64
            && (!allow_escape_timeout || since.elapsed() < Duration::from_millis(25))
        {
            return None;
        }
        return Some((1, "\x1b".to_owned()));
    }
    let width = match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    let available = &bytes[..width.min(bytes.len())];
    match std::str::from_utf8(available) {
        Ok("\n") => Some((1, "\r".to_owned())),
        Ok("\x08") => Some((1, "\x7f".to_owned())),
        Ok(text) => Some((width, text.to_owned())),
        Err(error) if error.error_len().is_none() && !eof => None,
        Err(error) => Some((
            error.error_len().unwrap_or(available.len()),
            "\u{fffd}".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_csi_control, parse_kitty_control, pixel_size, Control};

    #[test]
    fn fragmented_kitty_reply_waits_for_terminator() {
        assert!(parse_kitty_control(b"\x1b_Gi=7;OK\x1b").is_none());
        assert_eq!(
            parse_kitty_control(b"\x1b_Gi=7;OK\x1b\\"),
            Some((
                11,
                Control::Kitty {
                    parameters: b"i=7".to_vec(),
                    payload: b"OK".to_vec(),
                }
            ))
        );
    }

    #[test]
    fn pixel_response_requires_real_dimensions() {
        assert_eq!(pixel_size(b"\x1b[4;900;1440t"), Some((1440, 900)));
        assert_eq!(pixel_size(b"\x1b[4;0;1440t"), None);
    }

    #[test]
    fn protocol_responses_are_distinct_from_keys() {
        assert!(matches!(
            parse_csi_control(b"\x1b[4;900;1440t"),
            Some((_, Control::Csi(_)))
        ));
        assert!(parse_csi_control(b"\x1b[A").is_none());
    }
}
