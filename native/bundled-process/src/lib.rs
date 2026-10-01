//! Run only configured executable basenames beside the application executable.
//! Application arguments and policy belong to the caller, not this plugin.

use serde::Serialize;
use std::{
    env,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime, State,
};

struct BundledProcess {
    directory: PathBuf,
    executables: Vec<String>,
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
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
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
            child: command
                .spawn()
                .map_err(|error| format!("Cannot spawn bundled executable: {error}"))?,
            reaped: false,
        };
        let mut stdin = child
            .child
            .stdin
            .take()
            .ok_or("Child stdin is unavailable")?;
        let mut stdout = child
            .child
            .stdout
            .take()
            .ok_or("Child stdout is unavailable")?;
        let mut stderr = child
            .child
            .stderr
            .take()
            .ok_or("Child stderr is unavailable")?;
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

pub fn init<R: Runtime>(executables: &[&str]) -> TauriPlugin<R> {
    let executables: Vec<String> = executables.iter().map(|name| (*name).to_owned()).collect();
    Builder::new("bundled-process")
        .invoke_handler(tauri::generate_handler![run])
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
            Ok(())
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pipe_child_fixture() {
        let Ok(mode) = env::var(FIXTURE_MODE) else {
            return;
        };
        if mode == "early" {
            io::stderr().write_all(b"input rejected").unwrap();
            std::process::exit(17);
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
