//! Native LTKBridge server for Rose on macOS.
//!
//! On Windows, Rose drives LTK Manager through a DLL injected into it, which
//! serves a TCP JSON bridge and reaches the backend over WebView2's DevTools
//! protocol. macOS's WKWebView has no such protocol, so this build serves the
//! same bridge itself and calls its own commands directly.
//!
//! The server listens on 127.0.0.1 only, on the first free port in
//! 46321..=46340 (or `LTK_API_PORT`), and publishes the port and a fresh token
//! as `port.txt` and `token.txt` in `~/Library/Application Support/LTKBridge`,
//! where Rose looks for them.

mod backend;
pub(crate) mod protocol;

use std::io::Write;
use std::ops::RangeInclusive;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use fs_err as fs;
use fs_err::os::unix::fs::OpenOptionsExt;
use tauri::AppHandle;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use self::protocol::Bridge;

const PORT_RANGE: RangeInclusive<u16> = 46321..=46340;

/// How much one client may ask of the bridge. `hello` and `ping` need no
/// token, so these bound what any local process can make the app hold.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    /// A longer request line closes the connection (the Windows bridge's
    /// 64 MiB, room for a mod sent as base64).
    pub max_line: usize,
    /// Connections served at once; more are closed on accept.
    pub max_connections: usize,
}

const LIMITS: Limits = Limits {
    max_line: 64 << 20,
    max_connections: 16,
};

/// Where this bridge published its files, and on which port, for the quit.
static PUBLISHED: OnceLock<(PathBuf, u16)> = OnceLock::new();

/// Start the bridge in the background. Failures are logged; the app runs on.
pub(crate) fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = serve(app).await {
            tracing::warn!("Rose bridge stopped: {e}");
        }
    });
}

async fn serve(app: AppHandle) -> std::io::Result<()> {
    let range = match std::env::var("LTK_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
    {
        Some(port) => port..=port,
        None => PORT_RANGE,
    };
    let (listener, port) = bind_listener(range).await?;
    let token = uuid::Uuid::new_v4().simple().to_string();

    let dir = bridge_dir(&app)
        .ok_or_else(|| std::io::Error::other("no app data directory for the bridge files"))?;
    publish_port_and_token(&dir, port, &token)?;
    let _ = PUBLISHED.set((dir.clone(), port));
    tracing::info!("Rose bridge listening on 127.0.0.1:{port}");

    let backend = Arc::new(backend::TauriBackend::new(app, dir));
    let bridge = Arc::new(Bridge::new(token, port, std::process::id(), backend));
    accept_loop(listener, bridge, LIMITS).await
}

/// Serve every connection `listener` accepts, at most `limits.max_connections`
/// at once.
pub(crate) async fn accept_loop(
    listener: TcpListener,
    bridge: Arc<Bridge>,
    limits: Limits,
) -> std::io::Result<()> {
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let bridge = bridge.clone();
        tokio::spawn(async move {
            serve_connection(stream, bridge, limits.max_line).await;
            drop(slot);
        });
    }
}

/// Answer each request line on one connection, in order. A line longer than
/// `max_line` closes the connection.
async fn serve_connection(stream: TcpStream, bridge: Arc<Bridge>, max_line: usize) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    loop {
        let mut raw = Vec::new();
        let limit = max_line as u64 + 1;
        match (&mut reader).take(limit).read_until(b'\n', &mut raw).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if raw.last() != Some(&b'\n') && raw.len() > max_line {
            return;
        }
        let Ok(line) = String::from_utf8(raw) else {
            return;
        };
        let line = line.trim_end_matches(['\n', '\r']).to_owned();
        if line.is_empty() {
            continue;
        }
        let bridge = bridge.clone();
        let Ok(answer) = tokio::task::spawn_blocking(move || bridge.handle_line(&line)).await
        else {
            return;
        };
        let mut out = answer.to_string();
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Bind 127.0.0.1 on the first port of `range` that is free.
pub(crate) async fn bind_listener(
    range: RangeInclusive<u16>,
) -> std::io::Result<(TcpListener, u16)> {
    let mut last_error = None;
    for port in range {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok((listener, port)),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("empty port range")))
}

/// `~/Library/Application Support/LTKBridge`: beside this app's data folder
/// and Rose's, which is where Rose reads it from.
fn bridge_dir(app: &AppHandle) -> Option<PathBuf> {
    crate::state::get_app_data_dir(app)?
        .parent()
        .map(|support| support.join("LTKBridge"))
}

/// Write `port.txt` and `token.txt` atomically into a folder only the owner
/// can open, each file readable by the owner only from its creation.
pub(crate) fn publish_port_and_token(dir: &Path, port: u16, token: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    write_private(&dir.join("port.txt"), &format!("{port}\n"))?;
    write_private(&dir.join("token.txt"), &format!("{token}\n"))
}

/// Remove the published files, so Rose does not try a bridge that is gone.
pub(crate) fn unpublish(dir: &Path) {
    let _ = fs::remove_file(dir.join("port.txt"));
    let _ = fs::remove_file(dir.join("token.txt"));
}

/// Remove the published files if they are still this bridge's (`port`):
/// a bridge started since keeps its own.
pub(crate) fn unpublish_if_ours(dir: &Path, port: u16) {
    let published = fs::read_to_string(dir.join("port.txt")).unwrap_or_default();
    if published.trim() == port.to_string() {
        unpublish(dir);
    }
}

/// On the app's exit: take this bridge's files down.
pub(crate) fn unpublish_on_exit() {
    if let Some((dir, port)) = PUBLISHED.get() {
        unpublish_if_ours(dir, *port);
    }
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(contents.as_bytes())?;
    drop(file);
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests;
