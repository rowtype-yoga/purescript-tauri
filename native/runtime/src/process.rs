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

#[derive(Debug)]
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
    let mut command = application_command(executable, arguments, source.is_some());
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

/// Launch an application task whose source delivery and descendants are owned
/// by this call. Unlike `run_application`, it never detaches from its child.
pub(crate) fn run_application_task(
    executable: &Path,
    arguments: &[String],
    source: Option<String>,
    interrupted: Arc<AtomicBool>,
) -> HostResult<Outcome> {
    if interrupted.load(Ordering::Relaxed) {
        return Err(failure("interrupted"));
    }
    let command = application_command(executable, arguments, source.is_some());
    let mut task = ApplicationTask::spawn(command, source)?;
    task.wait(interrupted, executable)
}

fn application_command(executable: &Path, arguments: &[String], has_source: bool) -> Command {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(if has_source {
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
    command
}

struct TaskInput {
    complete: mpsc::Receiver<Result<(), String>>,
    writer: JoinHandle<()>,
    done: bool,
    failure: Option<String>,
}

/// A task owns both its child group and its stdin writer. Keeping the writer
/// separate makes a blocked pipe interruptible without copying the source.
struct ApplicationTask {
    child: Child,
    input: Option<TaskInput>,
    status: Option<ExitStatus>,
}

impl ApplicationTask {
    fn spawn(mut command: Command, source: Option<String>) -> HostResult<Self> {
        let mut child = command.spawn().map_err(|error| {
            failure(format!(
                "{}: {error}",
                command.get_program().to_string_lossy()
            ))
        })?;
        let input = match source {
            None => None,
            Some(source) => {
                let mut stdin = match child.stdin.take() {
                    Some(stdin) => stdin,
                    None => {
                        kill_process_group(&mut child);
                        let _ = child.wait();
                        return Err(failure("application stdin pipe is unavailable"));
                    }
                };
                let (delivered, complete) = mpsc::sync_channel(1);
                let writer = match thread::Builder::new()
                    .name("native-application-stdin".into())
                    .spawn(move || {
                        let result = stdin
                            .write_all(source.as_bytes())
                            .map_err(|error| error.to_string());
                        let _ = delivered.send(result);
                    }) {
                    Ok(writer) => writer,
                    Err(error) => {
                        kill_process_group(&mut child);
                        let _ = child.wait();
                        return Err(error.into());
                    }
                };
                Some(TaskInput {
                    complete,
                    writer,
                    done: false,
                    failure: None,
                })
            }
        };
        Ok(Self {
            child,
            input,
            status: None,
        })
    }

    fn wait(&mut self, interrupted: Arc<AtomicBool>, executable: &Path) -> HostResult<Outcome> {
        loop {
            if interrupted.load(Ordering::Relaxed) {
                self.cancel()?;
                return Err(failure("interrupted"));
            }
            if let Some(status) = self.child.try_wait()? {
                self.status = Some(status);
                // Descendants may retain stdin after the leader exits. The task
                // owns that group, so close it before joining the writer.
                kill_process_group(&mut self.child);
                let delivered = self.wait_input();
                if status.success() {
                    if let Err(error) = delivered {
                        return Err(failure(format!(
                            "writing {} stdin: {error}",
                            executable.display()
                        )));
                    }
                }
                return Ok(Outcome {
                    success: status.success(),
                    code: status.code().unwrap_or(-1),
                    stderr: String::new(),
                });
            }
            if let Some(error) = self.poll_input_failure() {
                self.cancel()?;
                return Err(failure(format!(
                    "writing {} stdin: {error}",
                    executable.display()
                )));
            }
            thread::sleep(POLL);
        }
    }

    fn poll_input_failure(&mut self) -> Option<String> {
        let input = self.input.as_mut()?;
        if !input.done {
            match input.complete.try_recv() {
                Ok(Ok(())) => input.done = true,
                Ok(Err(error)) => {
                    input.done = true;
                    input.failure = Some(error);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    input.done = true;
                    input.failure = Some("application stdin writer stopped".into());
                }
            }
        }
        input.failure.take()
    }

    fn wait_input(&mut self) -> HostResult<()> {
        let Some(mut input) = self.input.take() else {
            return Ok(());
        };
        input
            .writer
            .join()
            .map_err(|_| failure("application stdin writer panicked"))?;
        if !input.done {
            input.done = true;
            input.failure = match input.complete.recv() {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some("application stdin writer stopped".into()),
            };
        }
        match input.failure.take() {
            Some(error) => Err(failure(error)),
            None => Ok(()),
        }
    }

    fn cancel(&mut self) -> HostResult<()> {
        kill_process_group(&mut self.child);
        let waited = if self.status.is_none() {
            self.child.wait().map(|status| {
                self.status = Some(status);
            })
        } else {
            Ok(())
        };
        // The delivery result is reported by wait(). Cancellation still joins
        // this writer so no blocked stdin thread can outlive the task.
        let _ = self.wait_input();
        if let Err(error) = waited {
            return Err(error.into());
        }
        Ok(())
    }
}

impl Drop for ApplicationTask {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.cancel();
        }
    }
}

fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // The group ID was created by Command::process_group(0) for this
        // owned child. Never signal the parent's terminal process group.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // CREATE_NEW_PROCESS_GROUP does not make Child::kill recursive.
        // taskkill terminates the child tree before the writer is joined.
        let child_id = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", child_id.as_str(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
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

#[cfg(all(test, unix))]
mod tests {
    use super::{application_command, run_application, run_application_task, ApplicationTask};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    static NEXT_TEMPORARY_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new() -> Self {
            let suffix = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tauri-native-process-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("temporary directory is available");
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn shell(script: String) -> Vec<String> {
        vec!["-c".into(), script]
    }

    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            if let Ok(contents) = fs::read_to_string(path) {
                if !contents.is_empty() {
                    return contents;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_group_is_gone(group: i32) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            if unsafe { libc::kill(-group, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "owned process group {group} survived cancellation"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn recorded_group(contents: &str) -> i32 {
        contents
            .trim()
            .split_whitespace()
            .next()
            .expect("shell recorded its process group leader")
            .parse()
            .expect("shell process group leader is numeric")
    }

    #[test]
    fn application_task_returns_child_exit_status() {
        let interrupted = Arc::new(AtomicBool::new(false));
        let success = run_application_task(
            Path::new("/bin/sh"),
            &shell("exit 0".into()),
            None,
            interrupted.clone(),
        )
        .expect("successful task exits normally");
        assert!(success.success);
        assert_eq!(success.code, 0);

        let failure = run_application_task(
            Path::new("/bin/sh"),
            &shell("exit 17".into()),
            None,
            interrupted,
        )
        .expect("nonzero task exit is reported");
        assert!(!failure.success);
        assert_eq!(failure.code, 17);
    }

    #[test]
    fn application_task_preserves_rejection_during_source_delivery() {
        let executable = Path::new("/bin/sh");
        let command = application_command(executable, &shell("exit 17".into()), true);
        let mut task = ApplicationTask::spawn(command, Some("source".repeat(128 * 1024)))
            .expect("task starts with piped input");
        task.child.wait().expect("child rejects the request");
        let outcome = task
            .wait(Arc::new(AtomicBool::new(false)), executable)
            .expect("child rejection is not replaced by a broken stdin error");
        assert!(!outcome.success);
        assert_eq!(outcome.code, 17);
    }

    #[test]
    fn interrupted_task_reaps_its_process_group() {
        let temporary = TemporaryDirectory::new();
        let pids = temporary.file("pids");
        let arguments = shell(format!(
            "sleep 30 & child=$!; printf '%s %s' \"$$\" \"$child\" > '{}'; wait",
            pids.display()
        ));
        let interrupted = Arc::new(AtomicBool::new(false));
        let wait_flag = interrupted.clone();
        let wait = thread::spawn(move || {
            run_application_task(
                Path::new("/bin/sh"),
                &arguments,
                Some("source".repeat(1024 * 1024)),
                wait_flag,
            )
            .map_err(|error| error.to_string())
        });
        let group = recorded_group(&wait_for_file(&pids));
        interrupted.store(true, Ordering::Relaxed);
        let error = wait
            .join()
            .expect("task wait thread does not panic")
            .expect_err("interrupt fails the owned task");
        assert_eq!(error.to_string(), "interrupted");
        assert_group_is_gone(group);
    }

    #[test]
    fn source_delivery_failure_reaps_its_process_group() {
        let temporary = TemporaryDirectory::new();
        let pids = temporary.file("pids");
        let close_stdin = temporary.file("close-stdin");
        let arguments = shell(format!(
            "sleep 30 <&- & child=$!; printf '%s %s' \"$$\" \"$child\" > '{}'; \
             while [ ! -f '{}' ]; do sleep 0.01; done; exec 0<&-; wait",
            pids.display(),
            close_stdin.display()
        ));
        let wait = thread::spawn(move || {
            run_application_task(
                Path::new("/bin/sh"),
                &arguments,
                Some("source".repeat(1024 * 1024)),
                Arc::new(AtomicBool::new(false)),
            )
            .map_err(|error| error.to_string())
        });
        let group = recorded_group(&wait_for_file(&pids));
        fs::write(close_stdin, "").expect("close-stdin sentinel is writable");
        let error = wait
            .join()
            .expect("task wait thread does not panic")
            .expect_err("closed stdin fails source delivery");
        assert!(
            error.to_string().starts_with("writing /bin/sh stdin:"),
            "unexpected source error: {error}"
        );
        assert_group_is_gone(group);
    }

    #[test]
    fn interactive_application_detaches_on_interrupt() {
        let temporary = TemporaryDirectory::new();
        let started = temporary.file("started");
        let continue_application = temporary.file("continue");
        let finished = temporary.file("finished");
        let arguments = shell(format!(
            "printf started > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; \
             printf finished > '{}'",
            started.display(),
            continue_application.display(),
            finished.display()
        ));
        let interrupted = Arc::new(AtomicBool::new(false));
        let wait_flag = interrupted.clone();
        let wait = thread::spawn(move || {
            run_application(Path::new("/bin/sh"), &arguments, None, wait_flag)
                .map_err(|error| error.to_string())
        });
        wait_for_file(&started);
        interrupted.store(true, Ordering::Relaxed);
        let error = wait
            .join()
            .expect("interactive wait thread does not panic")
            .expect_err("interrupt returns from the interactive wait");
        assert_eq!(error.to_string(), "interrupted");
        fs::write(continue_application, "").expect("continue sentinel is writable");
        assert_eq!(wait_for_file(&finished), "finished");
    }
}
