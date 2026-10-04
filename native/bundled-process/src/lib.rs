//! Run only configured executable basenames beside the application executable.
//! Application arguments and policy belong to the caller, not this plugin.

use parking_lot::Mutex;
use serde::Serialize;
use shared_child::SharedChild;
use std::{
    collections::HashMap,
    env,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
};
use tauri::{
    ipc::{InvokeBody, Request},
    plugin::{Builder, TauriPlugin},
    webview::PageLoadEvent,
    AppHandle, Manager, RunEvent, Runtime, State, Webview, WindowEvent,
};

const STREAM_HEADER: &str = "x-tauri-bundled-process-stream";

struct BundledProcess {
    directory: PathBuf,
    executables: Vec<String>,
}

#[derive(Default)]
struct Streams {
    active: Mutex<HashMap<String, Arc<Stream>>>,
}

fn validate_basename(name: &str) -> Result<(), String> {
    // Reject Windows separators/drive prefixes even on Unix, and vice versa.
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':', '\0']) {
        return Err("Executable must be a single basename, not a path".into());
    }
    // Windows Command otherwise invokes cmd.exe implicitly for batch files.
    #[cfg(windows)]
    if name
        .trim_end_matches([' ', '.'])
        .rsplit('.')
        .next()
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        })
    {
        return Err("Batch files are not native executables".into());
    }
    Ok(())
}

impl BundledProcess {
    fn resolve(&self, executable: &str) -> Result<PathBuf, String> {
        validate_basename(executable)?;
        if !self.executables.iter().any(|allowed| allowed == executable) {
            return Err(format!("Executable is not allowlisted: {executable}"));
        }
        Ok(self.directory.join(executable))
    }
}

#[derive(Debug, Serialize)]
struct ProcessResult {
    success: bool,
    code: i32,
    stdout: String,
    stderr: String,
}

// Every exit path, including pipe-worker spawn errors and panics, reaps the child.
struct ChildGuard {
    child: Option<Child>,
    reaped: bool,
}

impl ChildGuard {
    fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self
            .child
            .as_mut()
            .expect("child guard always owns a child before reaping")
            .wait()?;
        self.reaped = true;
        Ok(status)
    }

    fn kill_and_reap(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let _ = child.kill();
        if child.wait().is_ok() {
            self.reaped = true;
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill_and_reap();
        }
    }
}

enum PipeOutput {
    Stdin,
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

fn pipe_worker<'scope, 'env, F>(
    scope: &'scope thread::Scope<'scope, 'env>,
    sender: &mpsc::Sender<Result<PipeOutput, String>>,
    name: &'static str,
    work: F,
) -> Result<(), String>
where
    F: FnOnce() -> io::Result<PipeOutput> + Send + 'scope,
{
    let sender = sender.clone();
    thread::Builder::new()
        .spawn_scoped(scope, move || {
            let result = work().map_err(|error| format!("Cannot transfer child {name}: {error}"));
            let _ = sender.send(result);
        })
        .map_err(|error| format!("Cannot start child {name} worker: {error}"))?;
    Ok(())
}

fn decode_output(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

fn execute(mut command: Command, input: String) -> Result<ProcessResult, String> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    thread::scope(|scope| {
        // Keep the guard inside the scope: on error, kill before scope joins
        // workers that may otherwise remain blocked on the child's pipes.
        let mut child = ChildGuard {
            child: Some(
                command
                    .spawn()
                    .map_err(|error| format!("Cannot spawn bundled executable: {error}"))?,
            ),
            reaped: false,
        };
        let process = child
            .child
            .as_mut()
            .expect("child guard always owns a child before reaping");
        let mut stdin = process.stdin.take().ok_or("Child stdin is unavailable")?;
        let mut stdout = process.stdout.take().ok_or("Child stdout is unavailable")?;
        let mut stderr = process.stderr.take().ok_or("Child stderr is unavailable")?;
        let (sender, receiver) = mpsc::channel();
        pipe_worker(scope, &sender, "stdin", move || {
            // Closing stdin is required even for empty input. A child is allowed
            // to stop consuming input early; preserve its exit status and stderr.
            match stdin.write_all(input.as_bytes()) {
                Err(error) if error.kind() != io::ErrorKind::BrokenPipe => return Err(error),
                _ => {}
            }
            drop(stdin);
            Ok(PipeOutput::Stdin)
        })?;
        pipe_worker(scope, &sender, "stdout", move || {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output)?;
            Ok(PipeOutput::Stdout(output))
        })?;
        pipe_worker(scope, &sender, "stderr", move || {
            let mut output = Vec::new();
            stderr.read_to_end(&mut output)?;
            Ok(PipeOutput::Stderr(output))
        })?;
        drop(sender);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for _ in 0..3 {
            match receiver
                .recv()
                .map_err(|error| format!("Child pipe worker failed: {error}"))??
            {
                PipeOutput::Stdin => {}
                PipeOutput::Stdout(output) => stdout = output,
                PipeOutput::Stderr(output) => stderr = output,
            }
        }
        let status = child
            .wait()
            .map_err(|error| format!("Cannot wait for bundled executable: {error}"))?;
        Ok(ProcessResult {
            success: status.success(),
            code: status.code().unwrap_or(-1),
            stdout: decode_output(stdout),
            stderr: decode_output(stderr),
        })
    })
}

type OutputReader = JoinHandle<io::Result<Vec<u8>>>;

struct Stream {
    owner: String,
    // stdin has its own lock so cancellation can kill the child while a write is
    // blocked in the kernel. The stream registry lock is never held for I/O.
    stdin: Mutex<Option<ChildStdin>>,
    // SharedChild keeps waiting, killing, and reaping independently callable.
    // In particular, cancel can kill while finish is blocked in wait().
    child: Arc<SharedChild>,
    stdout: Mutex<Option<OutputReader>>,
    stderr: Mutex<Option<OutputReader>>,
    closing: AtomicBool,
    writing: AtomicBool,
    waiting: AtomicBool,
}

struct Writing<'a>(&'a AtomicBool);

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Waiting<'a>(&'a AtomicBool);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Stream {
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        if self.closing.load(Ordering::Acquire) {
            return Err("Process stream is closing".into());
        }
        self.writing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "A process stream write is already in flight".to_owned())?;
        let _writing = Writing(&self.writing);
        let mut stdin = self.stdin.lock();
        if self.closing.load(Ordering::Acquire) {
            return Err("Process stream is closing".into());
        }
        let stdin = stdin.as_mut().ok_or("Process stream stdin is closed")?;
        stdin
            .write_all(bytes)
            .map_err(|error| format!("Cannot write child stdin: {error}"))
    }

    fn finish(&self) -> Result<ProcessResult, String> {
        if self.closing.swap(true, Ordering::AcqRel) {
            return Err("Process stream is already closing".into());
        }
        self.stdin.lock().take();
        let status = self.reap(false)?;
        let (stdout, stderr) = self.collect_output()?;
        Ok(ProcessResult {
            success: status.success(),
            code: status.code().unwrap_or(-1),
            stdout: decode_output(stdout),
            stderr: decode_output(stderr),
        })
    }

    fn cancel(&self) -> Result<(), String> {
        self.closing.store(true, Ordering::Release);
        self.reap(true)?;
        self.discard_output();
        Ok(())
    }

    fn abandon(&self) {
        self.closing.store(true, Ordering::Release);
        let _ = self.reap(true);
        self.discard_output();
    }

    fn reap(&self, kill: bool) -> Result<ExitStatus, String> {
        // SharedChild allows kill and wait from independent threads. This means
        // cancellation remains live while either a stdin write or finish wait
        // is blocked.
        if kill {
            let _ = self.child.kill();
        }
        self.waiting.store(true, Ordering::Release);
        let _waiting = Waiting(&self.waiting);
        self.child
            .wait()
            .map_err(|error| format!("Cannot wait for bundled executable: {error}"))
    }

    fn collect_output(&self) -> Result<(Vec<u8>, Vec<u8>), String> {
        let stdout = self
            .stdout
            .lock()
            .take()
            .ok_or("Process stream stdout is unavailable")?;
        let stderr = self
            .stderr
            .lock()
            .take()
            .ok_or("Process stream stderr is unavailable")?;
        let stdout = join_output(stdout, "stdout");
        let stderr = join_output(stderr, "stderr");
        Ok((stdout?, stderr?))
    }

    fn discard_output(&self) {
        if let Some(stdout) = self.stdout.lock().take() {
            let _ = stdout.join();
        }
        if let Some(stderr) = self.stderr.lock().take() {
            let _ = stderr.join();
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.abandon();
    }
}

fn join_output(reader: OutputReader, name: &str) -> Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| format!("Child {name} reader panicked"))?
        .map_err(|error| format!("Cannot read child {name}: {error}"))
}

fn output_reader<R: Read + Send + 'static>(
    name: &str,
    mut pipe: R,
) -> Result<OutputReader, String> {
    thread::Builder::new()
        .name(format!("bundled-process-{name}"))
        .spawn(move || {
            let mut output = Vec::new();
            pipe.read_to_end(&mut output)?;
            Ok(output)
        })
        .map_err(|error| format!("Cannot start child {name} reader: {error}"))
}

fn spawn_stream(mut command: Command, owner: String) -> Result<Arc<Stream>, String> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = Arc::new(
        SharedChild::spawn(&mut command)
            .map_err(|error| format!("Cannot spawn bundled executable: {error}"))?,
    );
    let stdin = child.take_stdin().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        "Child stdin is unavailable"
    })?;
    let stdout_pipe = child.take_stdout().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        "Child stdout is unavailable"
    })?;
    let stderr_pipe = child.take_stderr().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        "Child stderr is unavailable"
    })?;
    let stdout = match output_reader("stdout", stdout_pipe) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let stderr = match output_reader("stderr", stderr_pipe) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout.join();
            return Err(error);
        }
    };
    Ok(Arc::new(Stream {
        owner,
        stdin: Mutex::new(Some(stdin)),
        child,
        stdout: Mutex::new(Some(stdout)),
        stderr: Mutex::new(Some(stderr)),
        closing: AtomicBool::new(false),
        writing: AtomicBool::new(false),
        waiting: AtomicBool::new(false),
    }))
}

fn stream_id() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "Unable to obtain stream identifier randomness")?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 15) as usize] as char);
    }
    Ok(id)
}

fn insert_stream(streams: &Streams, stream: Arc<Stream>) -> Result<String, String> {
    let mut active = streams.active.lock();
    loop {
        let id = stream_id()?;
        if !active.contains_key(&id) {
            active.insert(id.clone(), stream);
            return Ok(id);
        }
    }
}

fn owned_stream<R: Runtime>(
    streams: &Streams,
    webview: &Webview<R>,
    id: &str,
) -> Result<Arc<Stream>, String> {
    let stream = streams
        .active
        .lock()
        .get(id)
        .cloned()
        .ok_or("Unknown process stream")?;
    if stream.owner != webview.label() {
        return Err("Process stream belongs to a different webview".into());
    }
    Ok(stream)
}

fn remove_stream(streams: &Streams, id: &str, stream: &Arc<Stream>) {
    let mut active = streams.active.lock();
    if active
        .get(id)
        .is_some_and(|current| Arc::ptr_eq(current, stream))
    {
        active.remove(id);
    }
}

fn requested_stream<'a>(request: &'a Request<'_>) -> Result<&'a str, String> {
    request
        .headers()
        .get(STREAM_HEADER)
        .ok_or("Missing process stream header")?
        .to_str()
        .map_err(|_| "Process stream header is not valid text".to_owned())
}

fn raw_stream_bytes<'a>(request: &'a Request<'_>) -> Result<&'a [u8], String> {
    if request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/octet-stream")
    {
        return Err("Process stream writes require application/octet-stream".into());
    }
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("Process stream writes require a raw ArrayBuffer body".into());
    };
    Ok(bytes)
}

fn write_native_stream(
    mut stream: impl Write,
    text: String,
    stream_name: &str,
) -> Result<(), String> {
    stream
        .write_all(text.as_bytes())
        .map_err(|error| format!("Cannot write application {stream_name}: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("Cannot flush application {stream_name}: {error}"))
}

fn take_streams(streams: &Streams, owner: Option<&str>) -> Vec<Arc<Stream>> {
    let mut active = streams.active.lock();
    let ids: Vec<String> = active
        .iter()
        .filter_map(|(id, stream)| {
            if owner.map_or(true, |owner| stream.owner == owner) {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();
    ids.into_iter()
        .filter_map(|id| active.remove(&id))
        .collect()
}

fn cleanup_streams(streams: Vec<Arc<Stream>>) {
    for stream in streams {
        stream.abandon();
    }
}

fn cleanup_owner<R: Runtime>(app: &AppHandle<R>, owner: Option<&str>) {
    cleanup_streams(take_streams(app.state::<Streams>().inner(), owner));
}

#[tauri::command]
async fn run(
    process: State<'_, BundledProcess>,
    executable: String,
    arguments: Vec<String>,
    stdin: String,
) -> Result<ProcessResult, String> {
    let executable = process.resolve(&executable)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut command = Command::new(executable);
        command.args(arguments);
        execute(command, stdin)
    })
    .await
    .map_err(|error| format!("Bundled process worker failed: {error}"))?
}

#[tauri::command]
async fn stdout(text: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let native_stdout = io::stdout();
        write_native_stream(native_stdout.lock(), text, "stdout")
    })
    .await
    .map_err(|error| format!("Bundled process worker failed: {error}"))?
}

#[tauri::command]
async fn stderr(text: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let native_stderr = io::stderr();
        write_native_stream(native_stderr.lock(), text, "stderr")
    })
    .await
    .map_err(|error| format!("Bundled process worker failed: {error}"))?
}

#[tauri::command]
fn spawn<R: Runtime>(
    process: State<'_, BundledProcess>,
    streams: State<'_, Streams>,
    webview: Webview<R>,
    executable: String,
    arguments: Vec<String>,
) -> Result<String, String> {
    let executable = process.resolve(&executable)?;
    let mut command = Command::new(executable);
    command.args(arguments);
    let stream = spawn_stream(command, webview.label().to_owned())?;
    insert_stream(streams.inner(), stream)
}

#[tauri::command]
async fn write<R: Runtime>(
    streams: State<'_, Streams>,
    webview: Webview<R>,
    request: Request<'_>,
) -> Result<(), String> {
    let id = requested_stream(&request)?;
    let stream = owned_stream(streams.inner(), &webview, id)?;
    // Request borrows Tauri's raw body, so moving the blocking pipe write off
    // the IPC task requires one owned frame copy. No JSON/base64 conversion or
    // additional stream buffering is introduced.
    let bytes = raw_stream_bytes(&request)?.to_vec();
    tauri::async_runtime::spawn_blocking(move || stream.write(&bytes))
        .await
        .map_err(|error| format!("Bundled process worker failed: {error}"))?
}

#[tauri::command]
async fn finish<R: Runtime>(
    streams: State<'_, Streams>,
    webview: Webview<R>,
    stream: String,
) -> Result<ProcessResult, String> {
    let stream_handle = owned_stream(streams.inner(), &webview, &stream)?;
    let result = match tauri::async_runtime::spawn_blocking({
        let stream_handle = Arc::clone(&stream_handle);
        move || stream_handle.finish()
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(format!("Bundled process worker failed: {error}")),
    };
    remove_stream(streams.inner(), &stream, &stream_handle);
    result
}

#[tauri::command]
async fn cancel<R: Runtime>(
    streams: State<'_, Streams>,
    webview: Webview<R>,
    stream: String,
) -> Result<(), String> {
    let stream_handle = owned_stream(streams.inner(), &webview, &stream)?;
    let result = match tauri::async_runtime::spawn_blocking({
        let stream_handle = Arc::clone(&stream_handle);
        move || stream_handle.cancel()
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(format!("Bundled process worker failed: {error}")),
    };
    remove_stream(streams.inner(), &stream, &stream_handle);
    result
}

pub fn init<R: Runtime>(executables: &[&str]) -> TauriPlugin<R> {
    let executables: Vec<String> = executables.iter().map(|name| (*name).to_owned()).collect();
    Builder::new("bundled-process")
        .invoke_handler(tauri::generate_handler![
            run, spawn, write, finish, cancel, stdout, stderr
        ])
        .setup(move |app, _| {
            for name in &executables {
                validate_basename(name)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            }
            let executable = env::current_exe()?;
            let directory = executable
                .parent()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "Application executable has no parent directory",
                    )
                })?
                .to_owned();
            app.manage(BundledProcess {
                directory,
                executables,
            });
            app.manage(Streams::default());
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), PageLoadEvent::Started) {
                cleanup_owner(&webview.app_handle(), Some(webview.label()));
            }
        })
        .on_event(|app, event| match event {
            RunEvent::Exit => cleanup_owner(app, None),
            RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } => cleanup_owner(app, Some(label)),
            _ => {}
        })
        .on_drop(|app| cleanup_owner(&app, None))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_until(label: &str, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for {label}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    const FIXTURE_MODE: &str = "TAURI_BUNDLED_PROCESS_TEST_CHILD";
    const PIPE_BYTES: usize = 512 * 1024;

    #[test]
    fn only_exact_allowlisted_basenames_resolve() {
        let process = BundledProcess {
            directory: env::current_exe().unwrap().parent().unwrap().to_owned(),
            executables: vec!["renderer".into(), "../escape".into()],
        };
        assert_eq!(
            process.resolve("renderer").unwrap(),
            process.directory.join("renderer")
        );
        for name in [
            "",
            ".",
            "..",
            "other",
            "Renderer",
            "./renderer",
            "../escape",
            "/renderer",
            "a/renderer",
            "a\\renderer",
            "C:renderer",
            "renderer\0",
        ] {
            assert!(process.resolve(name).is_err(), "accepted {name:?}");
        }
    }

    fn fixture(mode: &str) -> Command {
        // Reuse this test executable: no shell, PATH lookup, or fixture binary.
        let mut command = Command::new(env::current_exe().unwrap());
        command.args(["--exact", "tests::pipe_child_fixture", "--nocapture"]);
        command.env(FIXTURE_MODE, mode);
        command
    }

    #[test]
    fn large_bidirectional_pipes_preserve_nonzero_exit_and_stderr() {
        let result = execute(fixture("large"), "i".repeat(PIPE_BYTES * 4)).unwrap();
        assert!(!result.success);
        assert_eq!(result.code, 23);
        assert!(result.stdout.ends_with(&"o".repeat(PIPE_BYTES)));
        assert_eq!(result.stderr, "e".repeat(PIPE_BYTES));
    }

    #[test]
    fn empty_input_delivers_eof() {
        let result = execute(fixture("empty"), String::new()).unwrap();
        assert!(result.success);
        assert_eq!(result.code, 0);
        assert!(result.stdout.ends_with("received EOF"));
    }

    #[test]
    fn early_child_exit_preserves_status_instead_of_broken_pipe() {
        let result = execute(fixture("early"), "i".repeat(PIPE_BYTES * 4)).unwrap();
        assert!(!result.success);
        assert_eq!(result.code, 17);
        assert_eq!(result.stderr, "input rejected");
    }

    #[test]
    fn cancel_unblocks_a_blocked_stream_write() {
        let stream = spawn_stream(fixture("blocked-write"), "test".into()).unwrap();
        let writer_stream = Arc::clone(&stream);
        let writer = thread::spawn(move || writer_stream.write(&vec![b'i'; PIPE_BYTES * 16]));

        wait_until("the stream write to block", || {
            stream.writing.load(Ordering::Acquire)
        });
        let cancelled_at = Instant::now();
        stream.cancel().unwrap();

        assert!(writer.join().unwrap().is_err());
        assert!(cancelled_at.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancel_kills_a_child_while_finish_waits() {
        let stream = spawn_stream(fixture("finish-wait"), "test".into()).unwrap();
        let finish_stream = Arc::clone(&stream);
        let finish = thread::spawn(move || finish_stream.finish());

        wait_until("finish to begin waiting for the child", || {
            stream.waiting.load(Ordering::Acquire)
        });
        let cancelled_at = Instant::now();
        stream.cancel().unwrap();
        assert!(cancelled_at.elapsed() < Duration::from_secs(5));

        match finish.join().unwrap() {
            Ok(result) => assert!(!result.success),
            Err(_) => {}
        }
    }

    #[test]
    fn pipe_child_fixture() {
        let Ok(mode) = env::var(FIXTURE_MODE) else {
            return;
        };
        if mode == "early" {
            io::stderr().write_all(b"input rejected").unwrap();
            std::process::exit(17);
        }
        if mode == "blocked-write" {
            thread::sleep(Duration::from_secs(30));
            return;
        }
        if mode == "finish-wait" {
            let mut input = Vec::new();
            io::stdin().read_to_end(&mut input).unwrap();
            thread::sleep(Duration::from_secs(30));
            return;
        }
        if mode == "large" {
            // Fill both outputs before reading stdin: sequential I/O deadlocks.
            io::stdout().write_all(&vec![b'o'; PIPE_BYTES]).unwrap();
            io::stderr().write_all(&vec![b'e'; PIPE_BYTES]).unwrap();
        }
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input).unwrap();
        if mode == "large" {
            assert_eq!(input, vec![b'i'; PIPE_BYTES * 4]);
            std::process::exit(23);
        }
        assert_eq!(mode, "empty");
        assert!(input.is_empty());
        io::stdout().write_all(b"received EOF").unwrap();
        io::stdout().flush().unwrap();
        std::process::exit(0);
    }
}
