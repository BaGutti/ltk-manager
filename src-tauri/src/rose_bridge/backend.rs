//! The bridge's backend: LTK Manager's own commands, called directly.
//!
//! Each call goes through the same Tauri command the UI invokes, so the bridge
//! keeps every check the UI gets (no library change while the patcher runs,
//! unknown ids refused) and answers with the same IPC envelope.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use super::protocol::Backend;
use crate::commands::{self, PatcherConfig};
use crate::error::{AppResult, IpcResult};
use crate::mods::ModLibraryState;
use crate::patcher::{PatcherError, PatcherState};

pub(crate) struct TauriBackend {
    app: AppHandle,
    bridge_dir: PathBuf,
}

impl TauriBackend {
    pub(crate) fn new(app: AppHandle, bridge_dir: PathBuf) -> Self {
        Self { app, bridge_dir }
    }

    /// Tell an open window the library changed, as the UI's own edits do.
    fn announce_library_change(&self, response: &Value) {
        if response.get("ok").and_then(Value::as_bool) == Some(true) {
            self.app.state::<ModLibraryState>().0.announce_change();
        }
    }
}

fn envelope<T: Serialize>(result: IpcResult<T>) -> Value {
    serde_json::to_value(result).unwrap_or_else(
        |e| serde_json::json!({"ok": false, "error": format!("unserializable answer: {e}")}),
    )
}

impl Backend for TauriBackend {
    fn list(&self) -> Value {
        let app = &self.app;
        envelope(commands::get_installed_mods(app.state(), app.state()))
    }

    fn install(&self, path: &str) -> Value {
        let app = &self.app;
        let response = envelope(commands::install_mod(
            path.to_owned(),
            app.state(),
            app.state(),
            app.state(),
        ));
        self.announce_library_change(&response);
        response
    }

    fn toggle(&self, mod_id: &str, enabled: bool) -> Value {
        let app = &self.app;
        let response = envelope(commands::toggle_mod(
            mod_id.to_owned(),
            enabled,
            app.state(),
            app.state(),
            app.state(),
        ));
        self.announce_library_change(&response);
        response
    }

    fn uninstall(&self, mod_id: &str) -> Value {
        let app = &self.app;
        let response = envelope(commands::uninstall_mod(
            mod_id.to_owned(),
            app.state(),
            app.state(),
            app.state(),
        ));
        self.announce_library_change(&response);
        response
    }

    fn start_patcher(&self) -> Value {
        let app = &self.app;
        // The command starts a second session over a running one; Rose expects
        // ALREADY_RUNNING so it can stop the old session and retry.
        if app.state::<PatcherState>().is_running() {
            let refused: AppResult<()> = Err(PatcherError::AlreadyRunning.into());
            return envelope(IpcResult::from(refused));
        }
        let config = PatcherConfig {
            flags: None,
            workshop_projects: None,
            force_rebuild: None,
        };
        envelope(commands::start_patcher(
            config,
            app.clone(),
            app.state(),
            app.state(),
            app.state(),
            app.state(),
            app.state(),
        ))
    }

    fn stop_patcher(&self) -> Value {
        envelope(commands::stop_patcher(self.app.state()))
    }

    fn patcher_status(&self) -> Value {
        envelope(commands::get_patcher_status(self.app.state()))
    }

    fn check_mod_health(&self, mod_id: &str) -> Value {
        envelope(tauri::async_runtime::block_on(commands::check_mod_health(
            mod_id.to_owned(),
            self.app.clone(),
        )))
    }

    fn repair_mod(&self, mod_id: &str) -> Value {
        envelope(tauri::async_runtime::block_on(commands::repair_mod(
            mod_id.to_owned(),
            self.app.clone(),
        )))
    }

    fn prepare_patcher(&self) {
        let app = self.app.clone();
        std::thread::spawn(move || match crate::commands::prewarm_host(&app) {
            Ok(true) => tracing::info!("Rose bridge: started the patcher host ahead of the game"),
            Ok(false) => {}
            Err(e) => tracing::warn!("Rose bridge: could not start the patcher host early: {e}"),
        });
    }

    fn quit(&self) {
        let app = self.app.clone();
        let dir = self.bridge_dir.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            super::unpublish(&dir);
            app.exit(0);
        });
    }
}
