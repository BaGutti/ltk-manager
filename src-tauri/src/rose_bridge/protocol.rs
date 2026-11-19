//! The LTKBridge line protocol, free of Tauri.
//!
//! One JSON object per line in, one per line out. It is the protocol of the
//! Windows `LeagueToolFlop.dll` bridge that Rose's `injection/tools/bridge.py`
//! speaks, so the same client drives this native server unchanged: `hello` and
//! `ping` answer anyone, every other command needs the token, and most answers
//! wrap LTK Manager's own IPC envelope (`{ok, value}` / `{ok: false, error}`)
//! as `response`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use fs_err as fs;
use serde_json::{json, Map, Value};

/// What the bridge asks of LTK Manager. Each call blocks and returns the IPC
/// envelope the matching Tauri command would have answered.
pub(crate) trait Backend: Send + Sync {
    fn list(&self) -> Value;
    fn install(&self, path: &str) -> Value;
    fn toggle(&self, mod_id: &str, enabled: bool) -> Value;
    fn uninstall(&self, mod_id: &str) -> Value;
    fn start_patcher(&self) -> Value;
    fn stop_patcher(&self) -> Value;
    fn patcher_status(&self) -> Value;
    fn check_mod_health(&self, mod_id: &str) -> Value;
    fn repair_mod(&self, mod_id: &str) -> Value;
    /// Quit the app shortly after the answer has gone out.
    fn quit(&self);
    /// Get the patcher ready ahead of a game, without blocking: on macOS its
    /// host asks for the administrator password as it starts, which must be
    /// answered before the game loads its files.
    fn prepare_patcher(&self);
}

pub(crate) struct Bridge {
    token: String,
    port: u16,
    pid: u32,
    backend: Arc<dyn Backend>,
    prepared: AtomicBool,
}

const SERVICE: &str = "LTKBridge";
const VERSION: &str = "macos-native-1";
const UNAUTHORIZED: &str = "unauthorized: missing or wrong token";

impl Bridge {
    pub(crate) fn new(token: String, port: u16, pid: u32, backend: Arc<dyn Backend>) -> Self {
        Self {
            token,
            port,
            pid,
            backend,
            prepared: AtomicBool::new(false),
        }
    }

    /// Answer one request line. Blocks while the backend works.
    pub(crate) fn handle_line(&self, line: &str) -> Value {
        let Ok(Value::Object(request)) = serde_json::from_str::<Value>(line) else {
            return error("invalid JSON");
        };
        let cmd = request
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match cmd {
            "hello" => json!({
                "ok": true,
                "service": SERVICE,
                "version": VERSION,
                "pid": self.pid,
                "port": self.port,
                "auth": true,
            }),
            "ping" => json!({"ok": true, "pong": now_millis()}),
            _ if !self.authorized(&request) => error(UNAUTHORIZED),
            _ => {
                // Rose's first command opens its session: the password prompt
                // shows now, during champion select, not as the game loads
                if !self.prepared.swap(true, Ordering::SeqCst) {
                    self.backend.prepare_patcher();
                }
                self.handle_authorized(cmd, &request)
            }
        }
    }

    fn authorized(&self, request: &Map<String, Value>) -> bool {
        request
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|token| constant_time_eq(token.as_bytes(), self.token.as_bytes()))
    }

    fn handle_authorized(&self, cmd: &str, request: &Map<String, Value>) -> Value {
        match cmd {
            "status" => json!({
                "ok": true,
                "pid": self.pid,
                "cdp": {"port": 0, "available": true},
                "api": {"tcp_port": self.port, "tcp_ok": true},
                "league_game_path": Value::Null,
            }),
            "list" => self.list(),
            "import" => self.import(request),
            "toggle" => {
                let enabled = request
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                self.with_target(request, |id| self.backend.toggle(id, enabled))
            }
            "uninstall" => self.with_target(request, |id| self.backend.uninstall(id)),
            "start_patcher" => wrap(self.backend.start_patcher()),
            "stop_patcher" => wrap(self.backend.stop_patcher()),
            "patcher_status" => wrap(self.backend.patcher_status()),
            "check_mod_health" => self.with_mod_id(request, |id| self.backend.check_mod_health(id)),
            "repair_mod" => self.with_mod_id(request, |id| self.backend.repair_mod(id)),
            // This LTK Manager has no automatic health check to switch off.
            "health_check_off" => json!({"ok": true, "changed": false}),
            "kill" => {
                self.backend.quit();
                json!({"ok": true, "message": "terminating host process in 200 ms"})
            }
            other => error(&format!("unknown: {other}")),
        }
    }

    fn list(&self) -> Value {
        let raw = self.backend.list();
        match raw.get("value").filter(|_| envelope_ok(&raw)).cloned() {
            Some(library @ Value::Array(_)) => json!({"ok": true, "library": library, "raw": raw}),
            _ => json!({"ok": false, "raw": raw}),
        }
    }

    fn import(&self, request: &Map<String, Value>) -> Value {
        if let Some(path) = request.get("path").and_then(Value::as_str) {
            if !Path::new(path).is_file() {
                return error(&format!("not found: {path}"));
            }
            return import_answer(self.backend.install(path));
        }
        let Some(data) = request.get("data_b64").and_then(Value::as_str) else {
            return error("missing 'path' or 'data_b64'");
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
            return error("invalid base64 in 'data_b64'");
        };
        let name = request
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("ImportedMod.fantome");
        let staging = staging_dir();
        let file = staging.join(archive_file_name(name));
        let staged = fs::create_dir_all(&staging).and_then(|()| fs::write(&file, bytes));
        let answer = match staged {
            Ok(()) => import_answer(self.backend.install(&file.to_string_lossy())),
            Err(e) => error(&format!("failed to stage archive: {e}")),
        };
        let _ = fs::remove_dir_all(&staging);
        answer
    }

    /// Resolve `target` (an id, or a name or display name in any case) to a
    /// mod id, then run `action` on it.
    fn with_target(
        &self,
        request: &Map<String, Value>,
        action: impl FnOnce(&str) -> Value,
    ) -> Value {
        let Some(target) = request.get("target").and_then(Value::as_str) else {
            return error("missing 'target'");
        };
        let Some(mod_id) = resolve_target(&self.backend.list(), target) else {
            return error(&format!("mod not found: {target}"));
        };
        let response = action(&mod_id);
        json!({"ok": envelope_ok(&response), "modId": mod_id, "response": response})
    }

    fn with_mod_id(
        &self,
        request: &Map<String, Value>,
        action: impl FnOnce(&str) -> Value,
    ) -> Value {
        match request.get("modId").and_then(Value::as_str) {
            Some(mod_id) => wrap(action(mod_id)),
            None => error("missing 'modId'"),
        }
    }
}

fn resolve_target(list: &Value, target: &str) -> Option<String> {
    let mods = list.get("value")?.as_array()?;
    let id_of = |entry: &Value| entry.get("id").and_then(Value::as_str).map(str::to_owned);
    if let Some(found) = mods
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(target))
    {
        return id_of(found);
    }
    let wanted = target.to_lowercase();
    mods.iter()
        .find(|m| {
            ["displayName", "name"].iter().any(|key| {
                m.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.to_lowercase() == wanted)
            })
        })
        .and_then(id_of)
}

fn import_answer(response: Value) -> Value {
    let mut answer = json!({"ok": envelope_ok(&response), "response": response});
    if let Some(error) = answer["response"].get("error").cloned() {
        answer["error"] = error;
    }
    answer
}

fn wrap(response: Value) -> Value {
    json!({"ok": envelope_ok(&response), "response": response})
}

/// An envelope's `ok`, true when it carries none (as the Windows bridge reads it).
fn envelope_ok(envelope: &Value) -> bool {
    envelope.get("ok").and_then(Value::as_bool).unwrap_or(true)
}

fn error(message: &str) -> Value {
    json!({"ok": false, "error": message})
}

fn staging_dir() -> PathBuf {
    std::env::temp_dir()
        .join("LTKBridge_Staging")
        .join(uuid::Uuid::new_v4().simple().to_string())
}

/// A file name from the client's `name`, stripped of any directory part and
/// given a `.fantome` extension unless it names an archive format already.
fn archive_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    let base = if base.is_empty() || base == "." || base == ".." {
        "ImportedMod"
    } else {
        base
    };
    let lower = base.to_lowercase();
    if lower.ends_with(".fantome") || lower.ends_with(".modpkg") {
        base.to_owned()
    } else {
        format!("{base}.fantome")
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
