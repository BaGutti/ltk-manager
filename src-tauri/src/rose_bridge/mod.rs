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

use std::ops::RangeInclusive;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_err as fs;
use tauri::AppHandle;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use self::protocol::Bridge;

const PORT_RANGE: RangeInclusive<u16> = 46321..=46340;

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
    tracing::info!("Rose bridge listening on 127.0.0.1:{port}");

    let backend = Arc::new(backend::TauriBackend::new(app, dir));
    let bridge = Arc::new(Bridge::new(token, port, std::process::id(), backend));
    loop {
        let (stream, _) = listener.accept().await?;
        tauri::async_runtime::spawn(serve_connection(stream, bridge.clone()));
    }
}

/// Answer each request line on one connection, in order.
async fn serve_connection(stream: TcpStream, bridge: Arc<Bridge>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim_end_matches('\r').to_owned();
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

/// Write `port.txt` and `token.txt` atomically, readable by the owner only.
pub(crate) fn publish_port_and_token(dir: &Path, port: u16, token: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    write_private(&dir.join("port.txt"), &format!("{port}\n"))?;
    write_private(&dir.join("token.txt"), &format!("{token}\n"))
}

/// Remove the published files, so Rose does not try a bridge that is gone.
pub(crate) fn unpublish(dir: &Path) {
    let _ = fs::remove_file(dir.join("port.txt"));
    let _ = fs::remove_file(dir.join("token.txt"));
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests;
