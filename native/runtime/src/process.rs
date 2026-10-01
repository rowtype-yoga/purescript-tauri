use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{failure, HostResult};

const POLL: Duration = Duration::from_millis(10);

type Written = (Vec<u8>, io::Result<()>);

/// Owns an encoder's pipes, threads and process group. The single in-flight
/// write returns its allocation so the next frame can reuse its pixel buffer.
pub(crate) struct Process {
    child: Child,
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    written: mpsc::Receiver<Written>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<io::Result<Vec<u8>>>>,
    interrupted: Arc<AtomicBool>,
    status: Option<ExitStatus>,
}

pub(crate) struct Outcome {
    pub success: bool,
    pub code: i32,
    pub stderr: String,
}

/// An application owns its lifetime, unlike an encoder. The worker keeps
/// delivering source and reaps the child even after its caller stops waiting;
/// neither an interrupt nor an I/O error sends a signal to the application.
pub(crate) fn run_application(
    executable: &Path,
    arguments: &[String],
    source: Option<String>,
    interrupted: Arc<AtomicBool>,
) -> HostResult<Outcome> {
    if interrupted.load(Ordering::Relaxed) {
        return Err(failure("interrupted"));
    }
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(if source.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP disables inherited console CTRL_C events.
        command.creation_flags(0x0000_0200);
    }
    let executable = executable.display().to_string();
    let (complete, outcome) = mpsc::channel();
    let launch_interrupted = interrupted.clone();
    // Start the owner before launching the application: thread-creation failure
    // must not leave a child without a waiter. Everything moved here is owned.
    thread::Builder::new()
        .name("native-application".into())
        .spawn(move || {
            let report = |result: Result<Outcome, String>| {
                // Also report errors after the caller has detached. Always print
                // errors, avoiding a send/interrupt race that could hide a failure.
                if let Err(message) = &result {
                    let _ = writeln!(io::stderr().lock(), "{message}");
                }
                let _ = complete.send(result);
            };
            if launch_interrupted.load(Ordering::Relaxed) {
                report(Err("interrupted".into()));
                return;
            }
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    report(Err(format!("{executable}: {error}")));
                    return;
                }
            };
            let mut input_failed = false;
            if let Some(source) = source {
                // Output is inherited, not captured: writing stdin cannot deadlock
                // against a full stdout/stderr pipe. A blocked write remains on this
                // worker, so the invoking CLI can still respond to interruption.
                let delivery = match child.stdin.take() {
                    Some(mut stdin) => stdin.write_all(source.as_bytes()),
                    None => Err(io::Error::other("application stdin pipe is unavailable")),
                };
                // stdin has closed (EOF) before waiting, including on write failure.
                if let Err(error) = delivery {
                    input_failed = true;
                    report(Err(format!("writing {executable} stdin: {error}")));
                }
            }
            // A delivery error releases the caller immediately, but this worker
            // still waits without terminating an application with editable state.
            match child.wait() {
                Ok(status) if !input_failed => report(Ok(Outcome {
                    success: status.success(),
                    code: status.code().unwrap_or(-1),
                    stderr: String::new(),
                })),
                Ok(_) => {}
                Err(error) => report(Err(format!("waiting for {executable}: {error}"))),
            }
        })?;
    loop {
        match outcome.recv_timeout(POLL) {
            Ok(result) => return result.map_err(failure),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(failure("application waiter stopped"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if interrupted.load(Ordering::Relaxed) {
                    return Err(failure("interrupted"));
                }
            }
        }
    }
}

impl Process {
    pub fn spawn(
        executable: &Path,
        arguments: &[String],
        interrupted: Arc<AtomicBool>,
    ) -> HostResult<Self> {
        let mut command = Command::new(executable);
        command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // A separate process group prevents a terminal SIGINT from killing the
        // child before we can close pipes, reap it and restore the terminal.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|e| failure(format!("{}: {e}", executable.display())))?;
        let (input, requests) = mpsc::sync_channel::<Vec<u8>>(1);
        let (complete, written) = mpsc::sync_channel(1);
        let mut process = Self {
            child,
            input: None,
            written,
            writer: None,
            reader: None,
            interrupted,
            status: None,
        };
        let mut stderr = process
            .child
            .stderr
            .take()
            .ok_or_else(|| failure("child stderr pipe is unavailable"))?;
        process.reader = Some(thread::Builder::new().name("native-stderr".into()).spawn(
            move || {
                let mut bytes = Vec::new();
                stderr.read_to_end(&mut bytes)?;
                Ok(bytes)
            },
        )?);
        let mut stdin = process
            .child
            .stdin
            .take()
            .ok_or_else(|| failure("child stdin pipe is unavailable"))?;
        process.writer = Some(thread::Builder::new().name("native-stdin".into()).spawn(
            move || {
                while let Ok(bytes) = requests.recv() {
                    let result = stdin.write_all(&bytes);
                    let failed = result.is_err();
                    if complete.send((bytes, result)).is_err() || failed {
                        break;
                    }
                }
                // Dropping stdin delivers EOF, including on a write failure.
            },
        )?);
        process.input = Some(input);
        Ok(process)
    }

    pub fn write(&mut self, bytes: Vec<u8>) -> HostResult<Vec<u8>> {
        self.check_interrupt()?;
        let input = self
            .input
            .as_ref()
            .ok_or_else(|| failure("child input is closed"))?;
        if input.send(bytes).is_err() {
            return Err(self.write_error("child input writer stopped"));
        }
        loop {
            match self.written.recv_timeout(POLL) {
                Ok((bytes, Ok(()))) => return Ok(bytes),
                Ok((_, Err(error))) => return Err(self.write_error(&error.to_string())),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.write_error("child input writer stopped"))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => self.check_interrupt()?,
            }
        }
    }

    fn write_error(&mut self, message: &str) -> Box<dyn std::error::Error> {
        self.terminate();
        match self.take_stderr() {
            Ok(stderr) => failure(format!("writing child stdin: {message}\n{stderr}")),
            Err(error) => failure(format!(
                "writing child stdin: {message}; reading stderr: {error}"
            )),
        }
    }

    fn check_interrupt(&self) -> HostResult<()> {
        if self.interrupted.load(Ordering::Relaxed) {
            Err(failure("interrupted"))
        } else {
            Ok(())
        }
    }

    fn close_input(&mut self) -> HostResult<()> {
        self.input.take();
        if let Some(writer) = self.writer.take() {
            writer
                .join()
                .map_err(|_| failure("child stdin writer panicked"))?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> HostResult<Outcome> {
        self.close_input()?;
        let status = loop {
            self.check_interrupt()?;
            if let Some(status) = self.child.try_wait()? {
                self.status = Some(status);
                break status;
            }
            thread::sleep(POLL);
        };
        // Reap/close the child's remaining process group before joining readers:
        // descendants must not retain our pipe after the owning child exits.
        self.kill_group();
        let stderr = self.take_stderr()?;
        Ok(Outcome {
            success: status.success(),
            code: status.code().unwrap_or(-1),
            stderr,
        })
    }

    pub fn cancel(mut self) -> HostResult<()> {
        self.input.take();
        self.kill_group();
        if self.status.is_none() {
            self.status = Some(self.child.wait()?);
        }
        self.close_input()?;
        self.take_stderr()?;
        Ok(())
    }

    fn take_stderr(&mut self) -> HostResult<String> {
        match self.reader.take() {
            Some(reader) => {
                let bytes = reader
                    .join()
                    .map_err(|_| failure("child stderr reader panicked"))??;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
            None => Ok(String::new()),
        }
    }

    fn kill_group(&mut self) {
        #[cfg(unix)]
        {
            // The group ID was created by Command::process_group(0) for this
            // owned child. Never signal the parent's terminal process group.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        if self.status.is_none() {
            let _ = self.child.kill();
        }
    }

    fn terminate(&mut self) {
        self.input.take();
        self.kill_group();
        if self.status.is_none() {
            self.status = self.child.wait().ok();
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Only terminate a live ownership scope. finish() has already joined
        // both threads; avoid sending signals to a potentially reused PID.
        if self.writer.is_some() || self.reader.is_some() || self.status.is_none() {
            self.terminate();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }
}
