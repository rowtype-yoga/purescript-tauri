use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use parking_lot::Mutex;
use serde::Serialize;
use subtle::ConstantTimeEq;
use tauri::{
    plugin::{Builder, TauriPlugin},
    AppHandle, Emitter, EventTarget, Manager, RunEvent, Runtime, Webview, WindowEvent,
};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{timeout, Instant};

const EVENT: &str = "local-control://request";
const MAX_BODY: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_PENDING: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(5);

enum FrontendError {
    Handler,
    ResponseTooLarge,
}

type FrontendReply = Result<String, FrontendError>;

struct Pending {
    deadline: Instant,
    claimed: bool,
    reply: oneshot::Sender<FrontendReply>,
}

struct Session<R: Runtime> {
    app: AppHandle<R>,
    owner: String,
    window: String,
    id: String,
    authorization: String,
    host: String,
    discovery: PathBuf,
    active: AtomicBool,
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, Pending>>,
    shutdown: watch::Sender<bool>,
}

struct Control<R: Runtime>(Mutex<Option<Arc<Session<R>>>>);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlEvent<'a> {
    session: &'a str,
    id: &'a str,
    kind: &'a str,
    body: &'a str,
    deadline: f64,
}

#[derive(Serialize)]
struct Discovery<'a> {
    url: &'a str,
    token: &'a str,
    pid: u32,
}

impl<R: Runtime> Session<R> {
    fn emit(&self, id: &str, kind: &str, body: &str, deadline: f64) -> tauri::Result<()> {
        self.app.emit_to(
            EventTarget::Webview {
                label: self.owner.clone(),
            },
            EVENT,
            ControlEvent {
                session: &self.id,
                id,
                kind,
                body,
                deadline,
            },
        )
    }

    fn shutdown(&self) {
        if !self.active.swap(false, Ordering::AcqRel) {
            return;
        }
        // No request may be claimed once active is false. Drop reply senders
        // outside the mutex, and cancel any already-running frontend fibers.
        let pending = std::mem::take(&mut *self.pending.lock());
        for (id, _) in pending {
            let _ = self.emit(&id, "cancel", "", 0.0);
        }
        let _ = fs::remove_file(&self.discovery);
        let _ = self.shutdown.send(true);
    }
}

// Covers HTTP disconnection, deadline expiry, emission failure and server exit.
struct PendingGuard<R: Runtime> {
    session: Arc<Session<R>>,
    id: String,
}

impl<R: Runtime> Drop for PendingGuard<R> {
    fn drop(&mut self) {
        let removed = self.session.pending.lock().remove(&self.id);
        if removed.is_some() {
            let _ = self.session.emit(&self.id, "cancel", "", 0.0);
        }
    }
}

fn random_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "unable to obtain secure randomness")?;
    const HEX: &[u8] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 15) as usize] as char);
    }
    Ok(encoded)
}

fn write_discovery(directory: &Path, contents: &[u8]) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(|_| "unable to create application data directory")?;
    let directory = directory.join("control");
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&directory) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(_) => return Err("unable to create control discovery directory".into()),
    }
    let metadata = fs::symlink_metadata(&directory)
        .map_err(|_| "unable to inspect control discovery directory")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("control discovery directory is not a real directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Refuse a directory controlled by another local account.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("control discovery directory belongs to another account".into());
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|_| "unable to protect control discovery directory")?;
    }
    let destination = directory.join(format!("{}.json", std::process::id()));
    let temporary = directory.join(format!(".{}-{}.tmp", std::process::id(), random_secret()?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err("unable to publish control discovery file".into());
    }
    Ok(destination)
}

fn single_header<'a>(headers: &'a HeaderMap, name: header::HeaderName) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        None
    } else {
        Some(first)
    }
}

fn transport_error(status: StatusCode) -> Response {
    // No credentials, request bodies, or parser diagnostics escape in errors.
    let mut response = status.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn rpc<R: Runtime>(State(session): State<Arc<Session<R>>>, request: Request) -> Response {
    let headers = request.headers();
    if headers.contains_key(header::ORIGIN) {
        return transport_error(StatusCode::FORBIDDEN);
    }
    if single_header(headers, header::HOST) != Some(session.host.as_str())
        || request.uri().authority().is_some()
    {
        return transport_error(StatusCode::BAD_REQUEST);
    }
    let authenticated = single_header(headers, header::AUTHORIZATION)
        .map(|provided| bool::from(provided.as_bytes().ct_eq(session.authorization.as_bytes())))
        .unwrap_or(false);
    if !authenticated {
        return transport_error(StatusCode::UNAUTHORIZED);
    }
    if request.uri().path() != "/rpc" || request.uri().query().is_some() {
        return transport_error(StatusCode::NOT_FOUND);
    }
    if request.method() != Method::POST {
        return transport_error(StatusCode::METHOD_NOT_ALLOWED);
    }
    let content_type = single_header(headers, header::CONTENT_TYPE).unwrap_or("");
    if !content_type.eq_ignore_ascii_case("application/json")
        && !content_type.eq_ignore_ascii_case("application/json; charset=utf-8")
    {
        return transport_error(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if headers.contains_key(header::CONTENT_ENCODING) {
        return transport_error(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if !session.active.load(Ordering::Acquire) {
        return transport_error(StatusCode::SERVICE_UNAVAILABLE);
    }
    let id = session.next_id.fetch_add(1, Ordering::Relaxed).to_string();
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let deadline_wall = match SystemTime::now()
        .checked_add(REQUEST_TIMEOUT)
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
    {
        Some(time) => time.as_secs_f64() * 1000.0,
        None => return transport_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let (send, receive) = oneshot::channel();
    {
        let mut pending = session.pending.lock();
        if !session.active.load(Ordering::Acquire) {
            return transport_error(StatusCode::SERVICE_UNAVAILABLE);
        }
        if pending.len() >= MAX_PENDING {
            return transport_error(StatusCode::TOO_MANY_REQUESTS);
        }
        pending.insert(
            id.clone(),
            Pending {
                deadline,
                claimed: false,
                reply: send,
            },
        );
    }
    let _guard = PendingGuard {
        session: session.clone(),
        id: id.clone(),
    };
    let bytes = match timeout(BODY_TIMEOUT, to_bytes(request.into_body(), MAX_BODY)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return transport_error(StatusCode::PAYLOAD_TOO_LARGE),
        Err(_) => return transport_error(StatusCode::REQUEST_TIMEOUT),
    };
    let body = match std::str::from_utf8(&bytes) {
        Ok(body) => body,
        Err(_) => return transport_error(StatusCode::BAD_REQUEST),
    };
    if !session.active.load(Ordering::Acquire)
        || session.emit(&id, "request", body, deadline_wall).is_err()
    {
        return transport_error(StatusCode::SERVICE_UNAVAILABLE);
    }
    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(Ok(body))) if body.is_empty() => (
            StatusCode::NO_CONTENT,
            [(header::CACHE_CONTROL, "no-store")],
        )
            .into_response(),
        Ok(Ok(Ok(body))) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        Ok(Ok(Err(FrontendError::Handler))) => transport_error(StatusCode::INTERNAL_SERVER_ERROR),
        Ok(Ok(Err(FrontendError::ResponseTooLarge))) => (
            StatusCode::BAD_GATEWAY,
            [(header::CACHE_CONTROL, "no-store")],
            "Control response exceeds 8 MiB",
        )
            .into_response(),
        Ok(Err(_)) => transport_error(StatusCode::SERVICE_UNAVAILABLE),
        Err(_) => transport_error(StatusCode::GATEWAY_TIMEOUT),
    }
}

async fn run_server<R: Runtime>(
    listener: tokio::net::TcpListener,
    session: Arc<Session<R>>,
    mut stopped: watch::Receiver<bool>,
) {
    let router = Router::new().fallback(rpc::<R>).with_state(session);
    let mut connections = JoinSet::new();
    while !*stopped.borrow() {
        tokio::select! {
            _ = stopped.changed() => break,
            _ = connections.join_next(), if !connections.is_empty() => (),
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                // Bound connections before reading headers, not merely after
                // authentication. Idle/slow-header sockets cannot accumulate.
                if connections.len() >= MAX_CONNECTIONS {
                    continue;
                }
                let service = TowerToHyperService::new(router.clone());
                connections.spawn(async move {
                    let mut http = http1::Builder::new();
                    http.keep_alive(false)
                        .max_buf_size(16 * 1024)
                        .timer(TokioTimer::new())
                        .header_read_timeout(BODY_TIMEOUT);
                    // Also bounds slow response readers. One request per
                    // connection avoids persistent unauthenticated sockets.
                    let _ = timeout(
                        Duration::from_secs(40),
                        http.serve_connection(TokioIo::new(stream), service),
                    ).await;
                });
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

#[tauri::command]
async fn start<R: Runtime>(app: AppHandle<R>, webview: Webview<R>) -> Result<String, String> {
    let control = app.state::<Control<R>>();
    let mut current = control.0.lock();
    if current
        .as_ref()
        .is_some_and(|session| session.active.load(Ordering::Acquire))
    {
        return Err("local control is already running".into());
    }
    let token = random_secret()?;
    let id = random_secret()?;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|_| "unable to bind loopback control listener")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "unable to configure control listener")?;
    let address = listener
        .local_addr()
        .map_err(|_| "unable to read control listener address")?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|_| "unable to register control listener")?;
    let url = format!("http://{address}/rpc");
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|_| "application data directory is unavailable")?;
    let contents = serde_json::to_vec(&Discovery {
        url: &url,
        token: &token,
        pid: std::process::id(),
    })
    .map_err(|_| "unable to encode control discovery")?;
    let discovery = write_discovery(&data_dir, &contents)?;
    let (shutdown, stopped) = watch::channel(false);
    let session = Arc::new(Session {
        app: app.clone(),
        owner: webview.label().to_owned(),
        window: webview.window().label().to_owned(),
        id: id.clone(),
        authorization: format!("Bearer {token}"),
        host: address.to_string(),
        discovery,
        active: AtomicBool::new(true),
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        shutdown,
    });
    *current = Some(session.clone());
    tauri::async_runtime::spawn(async move {
        run_server(listener, session.clone(), stopped).await;
        let control = session.app.state::<Control<R>>();
        let mut current = control.0.lock();
        if current
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &session))
        {
            session.shutdown();
            *current = None;
        }
    });
    Ok(id)
}

#[tauri::command]
fn stop<R: Runtime>(app: AppHandle<R>, webview: Webview<R>, session: String) -> Result<(), String> {
    let control = app.state::<Control<R>>();
    let mut current = control.0.lock();
    if let Some(active) = current.as_ref() {
        if active.owner != webview.label() || active.id != session {
            return Err("control session does not belong to this webview".into());
        }
        active.shutdown();
        *current = None;
    }
    Ok(())
}

// The claim handshake prevents queued, expired or cancelled events from
// invoking frontend code. All three operations are scoped to the native caller.
#[tauri::command]
fn reply<R: Runtime>(
    app: AppHandle<R>,
    webview: Webview<R>,
    session: String,
    id: String,
    action: String,
    body: String,
) -> Result<bool, String> {
    let control = app.state::<Control<R>>();
    let active = {
        let current = control.0.lock();
        match current.as_ref() {
            Some(active) if active.owner == webview.label() && active.id == session => {
                active.clone()
            }
            _ => return Ok(false),
        }
    };
    let mut pending = active.pending.lock();
    if !active.active.load(Ordering::Acquire) {
        return Ok(false);
    }
    let Some(request) = pending.get_mut(&id) else {
        return Ok(false);
    };
    if Instant::now() >= request.deadline {
        return Ok(false);
    }
    if action == "claim" {
        if request.claimed {
            return Ok(false);
        }
        request.claimed = true;
        return Ok(true);
    }
    if !request.claimed || (action != "reply" && action != "fail") {
        return Ok(false);
    }
    let request = pending.remove(&id).unwrap();
    drop(pending);
    let result = if action != "reply" {
        Err(FrontendError::Handler)
    } else if body.len() > MAX_RESPONSE {
        Err(FrontendError::ResponseTooLarge)
    } else {
        Ok(body)
    };
    Ok(request.reply.send(result).is_ok())
}

fn stop_owned<R: Runtime>(app: &AppHandle<R>, owner_window: Option<&str>) {
    let control = app.state::<Control<R>>();
    let mut current = control.0.lock();
    if let Some(session) = current.as_ref() {
        if owner_window.map_or(true, |window| session.window == window) {
            session.shutdown();
            *current = None;
        }
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("local-control")
        .invoke_handler(tauri::generate_handler![start, stop, reply])
        .setup(|app, _| {
            app.manage(Control::<R>(Mutex::new(None)));
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Started) {
                let control = webview.app_handle().state::<Control<R>>();
                let mut current = control.0.lock();
                if current
                    .as_ref()
                    .is_some_and(|session| session.owner == webview.label())
                {
                    if let Some(session) = current.take() {
                        session.shutdown();
                    }
                }
            }
        })
        .on_event(|app, event| match event {
            RunEvent::Exit => stop_owned(app, None),
            RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } => {
                stop_owned(app, Some(label));
            }
            _ => (),
        })
        .on_drop(|app| stop_owned(&app, None))
        .build()
}
