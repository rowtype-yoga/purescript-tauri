//! Capture process launch arguments and authorize only individual files named by them.
//! Register alongside tauri-plugin-fs; the frontend still needs the relevant fs
//! command permissions. Argument interpretation belongs to the application.

use std::{
    env, fs,
    io::{self, Read},
    path::PathBuf,
};
use tauri::{
    plugin::{Builder, TauriPlugin},
    AppHandle, Manager, Runtime, State,
};
use tauri_plugin_fs::FsExt;

struct Launch {
    arguments: Vec<String>,
    cwd: PathBuf,
}

impl Launch {
    fn resolve_file_argument(&self, path: &str) -> Result<PathBuf, String> {
        // Compare the original spelling before resolving anything. A different
        // spelling of the same file is not a separately authorized argument.
        if !self.arguments.iter().any(|argument| argument == path) {
            return Err("Path is not an argument from this process launch".into());
        }

        let resolved = fs::canonicalize(self.cwd.join(path))
            .map_err(|error| format!("Cannot resolve launch file {path:?}: {error}"))?;
        let metadata = fs::metadata(&resolved)
            .map_err(|error| format!("Cannot inspect launch file {path:?}: {error}"))?;
        if !metadata.is_file() {
            return Err(format!("Launch path {path:?} is not a regular file"));
        }
        Ok(resolved)
    }
}

#[tauri::command]
fn arguments(launch: State<'_, Launch>) -> Vec<String> {
    launch.arguments.clone()
}

#[tauri::command]
async fn read_stdin() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut source = String::new();
        io::stdin()
            .read_to_string(&mut source)
            .map_err(|error| format!("Cannot read standard input: {error}"))?;
        Ok(source)
    })
    .await
    .map_err(|error| format!("Standard input worker failed: {error}"))?
}

#[tauri::command]
fn authorize_file_argument<R: Runtime>(
    app: AppHandle<R>,
    launch: State<'_, Launch>,
    path: String,
) -> Result<String, String> {
    let resolved = launch.resolve_file_argument(&path)?;
    let result = resolved
        .to_str()
        .ok_or("Resolved launch file path is not valid Unicode")?
        .to_owned();
    let scope = app
        .try_fs_scope()
        .ok_or("The filesystem plugin is not registered")?;
    // Grant only the resolved file, never a directory or a recursive scope.
    scope
        .allow_file(&resolved)
        .map_err(|error| format!("Cannot authorize launch file {path:?}: {error}"))?;
    Ok(result)
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("launch-file")
        .invoke_handler(tauri::generate_handler![
            arguments,
            authorize_file_argument,
            read_stdin
        ])
        .setup(|app, _| {
            let cwd = env::current_dir()?;
            let arguments = env::args_os()
                .skip(1)
                .map(|argument| {
                    argument.into_string().map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Launch argument is not valid Unicode",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            app.manage(Launch { arguments, cwd });
            Ok(())
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_launch_arguments_can_authorize_files() {
        let directory = tempfile::tempdir().unwrap();
        let selected = directory.path().join("selected.markgraf");
        let sibling = directory.path().join("sibling.markgraf");
        fs::write(&selected, "selected").unwrap();
        fs::write(&sibling, "sibling").unwrap();
        let launch = Launch {
            arguments: vec!["selected.markgraf".into()],
            cwd: directory.path().to_owned(),
        };

        assert_eq!(
            launch.resolve_file_argument("selected.markgraf").unwrap(),
            fs::canonicalize(&selected).unwrap()
        );
        assert!(launch.resolve_file_argument("sibling.markgraf").is_err());
        assert!(launch.resolve_file_argument("./selected.markgraf").is_err());
        assert!(launch
            .resolve_file_argument(selected.to_str().unwrap())
            .is_err());
    }

    #[test]
    fn launch_arguments_cannot_authorize_directories_or_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let launch = Launch {
            arguments: vec![".".into(), "missing.markgraf".into()],
            cwd: directory.path().to_owned(),
        };

        assert!(launch.resolve_file_argument(".").is_err());
        assert!(launch.resolve_file_argument("missing.markgraf").is_err());
    }
}
