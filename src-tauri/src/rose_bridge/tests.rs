use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::protocol::{Backend, Bridge};
use super::{bind_listener, publish_port_and_token};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Records every call and answers with canned IPC envelopes.
#[derive(Default)]
struct FakeBackend {
    calls: Mutex<Vec<String>>,
    mods: Value,
    patcher_running: bool,
}

impl FakeBackend {
    fn with_mods() -> Self {
        Self {
            mods: json!([
                {"id": "a1", "name": "lux-skin", "displayName": "Lux Skin", "enabled": false},
                {"id": "b2", "name": "map", "displayName": "Summoner Map", "enabled": true},
            ]),
            ..Self::default()
        }
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Backend for FakeBackend {
    fn list(&self) -> Value {
        self.record("list".into());
        json!({"ok": true, "value": self.mods})
    }
    fn install(&self, path: &str) -> Value {
        self.record(format!("install {path}"));
        let file_exists = std::path::Path::new(path).is_file();
        json!({"ok": true, "value": {"id": "new", "enabled": false, "staged": file_exists}})
    }
    fn toggle(&self, mod_id: &str, enabled: bool) -> Value {
        self.record(format!("toggle {mod_id} {enabled}"));
        json!({"ok": true, "value": null})
    }
    fn uninstall(&self, mod_id: &str) -> Value {
        self.record(format!("uninstall {mod_id}"));
        json!({"ok": true, "value": null})
    }
    fn start_patcher(&self) -> Value {
        self.record("start_patcher".into());
        if self.patcher_running {
            json!({"ok": false, "error": {"code": "PATCHER", "error": {"kind": "ALREADY_RUNNING"}}})
        } else {
            json!({"ok": true, "value": null})
        }
    }
    fn stop_patcher(&self) -> Value {
        self.record("stop_patcher".into());
        json!({"ok": true, "value": null})
    }
    fn patcher_status(&self) -> Value {
        self.record("patcher_status".into());
        json!({"ok": true, "value": {"running": true, "phase": "patching"}})
    }
    fn check_mod_health(&self, mod_id: &str) -> Value {
        self.record(format!("check {mod_id}"));
        json!({"ok": true, "value": {"health": "healthy"}})
    }
    fn repair_mod(&self, mod_id: &str) -> Value {
        self.record(format!("repair {mod_id}"));
        json!({"ok": true, "value": {"applied": 0}})
    }
    fn quit(&self) {
        self.record("quit".into());
    }
}

fn bridge(backend: FakeBackend) -> (Bridge, Arc<FakeBackend>) {
    let backend = Arc::new(backend);
    let bridge = Bridge::new(TOKEN.into(), 46321, 4242, backend.clone());
    (bridge, backend)
}

fn send(bridge: &Bridge, request: Value) -> Value {
    serde_json::from_str(&bridge.handle_line(&request.to_string()).to_string()).unwrap()
}

fn authed(mut request: Value) -> Value {
    request["token"] = json!(TOKEN);
    request
}

#[test]
fn hello_names_the_service_port_and_pid_without_a_token() {
    let (bridge, _) = bridge(FakeBackend::default());

    let answer = send(&bridge, json!({"cmd": "hello"}));

    assert_eq!(answer["ok"], json!(true));
    assert_eq!(answer["service"], json!("LTKBridge"));
    assert_eq!(answer["pid"], json!(4242));
    assert_eq!(answer["port"], json!(46321));
    assert_eq!(answer["auth"], json!(true));
}

#[test]
fn ping_answers_without_a_token() {
    let (bridge, _) = bridge(FakeBackend::default());

    let answer = send(&bridge, json!({"cmd": "ping"}));

    assert_eq!(answer["ok"], json!(true));
    assert!(answer["pong"].is_u64());
}

#[test]
fn a_command_without_the_token_is_refused_and_reaches_nothing() {
    let (bridge, backend) = bridge(FakeBackend::with_mods());

    for request in [
        json!({"cmd": "list"}),
        json!({"cmd": "list", "token": "wrong"}),
    ] {
        let answer = send(&bridge, request);
        assert_eq!(
            answer,
            json!({"ok": false, "error": "unauthorized: missing or wrong token"})
        );
    }
    assert!(backend.calls().is_empty());
}

#[test]
fn a_line_that_is_not_a_json_object_is_invalid() {
    let (bridge, _) = bridge(FakeBackend::default());

    let answer: Value = bridge.handle_line("not json");

    assert_eq!(answer, json!({"ok": false, "error": "invalid JSON"}));
}

#[test]
fn an_unknown_command_is_named_in_the_error() {
    let (bridge, _) = bridge(FakeBackend::default());

    let answer = send(&bridge, authed(json!({"cmd": "dance"})));

    assert_eq!(answer, json!({"ok": false, "error": "unknown: dance"}));
}

#[test]
fn list_returns_the_library_array_and_the_raw_envelope() {
    let (bridge, _) = bridge(FakeBackend::with_mods());

    let answer = send(&bridge, authed(json!({"cmd": "list"})));

    assert_eq!(answer["ok"], json!(true));
    assert_eq!(answer["library"].as_array().unwrap().len(), 2);
    assert_eq!(answer["raw"]["ok"], json!(true));
}

#[test]
fn toggle_resolves_a_display_name_case_insensitively() {
    let (bridge, backend) = bridge(FakeBackend::with_mods());

    let answer = send(
        &bridge,
        authed(json!({"cmd": "toggle", "target": "lux SKIN", "enabled": true})),
    );

    assert_eq!(answer["ok"], json!(true));
    assert_eq!(answer["modId"], json!("a1"));
    assert_eq!(answer["response"]["ok"], json!(true));
    assert_eq!(backend.calls(), vec!["list", "toggle a1 true"]);
}

#[test]
fn toggle_turns_a_mod_on_when_enabled_is_left_out() {
    let (bridge, backend) = bridge(FakeBackend::with_mods());

    send(&bridge, authed(json!({"cmd": "toggle", "target": "b2"})));

    assert_eq!(backend.calls(), vec!["list", "toggle b2 true"]);
}

#[test]
fn toggle_without_a_target_is_refused() {
    let (bridge, backend) = bridge(FakeBackend::with_mods());

    let answer = send(&bridge, authed(json!({"cmd": "toggle"})));

    assert_eq!(answer, json!({"ok": false, "error": "missing 'target'"}));
    assert!(backend.calls().is_empty());
}

#[test]
fn uninstall_of_an_unknown_mod_is_refused_without_reaching_the_backend() {
    let (bridge, backend) = bridge(FakeBackend::with_mods());

    let answer = send(
        &bridge,
        authed(json!({"cmd": "uninstall", "target": "nope"})),
    );

    assert_eq!(answer["ok"], json!(false));
    assert_eq!(answer["error"], json!("mod not found: nope"));
    assert_eq!(backend.calls(), vec!["list"]);
}

#[test]
fn uninstall_resolves_the_target_and_wraps_the_envelope() {
    let (bridge, backend) = bridge(FakeBackend::with_mods());

    let answer = send(
        &bridge,
        authed(json!({"cmd": "uninstall", "target": "map"})),
    );

    assert_eq!(answer["ok"], json!(true));
    assert_eq!(answer["modId"], json!("b2"));
    assert_eq!(backend.calls(), vec!["list", "uninstall b2"]);
}

#[test]
fn import_by_path_installs_that_file() {
    let (bridge, backend) = bridge(FakeBackend::default());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("skin.fantome");
    fs_err::write(&file, b"archive").unwrap();

    let answer = send(
        &bridge,
        authed(json!({"cmd": "import", "path": file.to_str().unwrap()})),
    );

    assert_eq!(answer["ok"], json!(true));
    assert_eq!(answer["response"]["value"]["id"], json!("new"));
    assert_eq!(
        backend.calls(),
        vec![format!("install {}", file.to_str().unwrap())]
    );
}

#[test]
fn import_of_a_missing_path_says_not_found() {
    let (bridge, backend) = bridge(FakeBackend::default());

    let answer = send(
        &bridge,
        authed(json!({"cmd": "import", "path": "/no/such/skin.fantome"})),
    );

    assert_eq!(
        answer,
        json!({"ok": false, "error": "not found: /no/such/skin.fantome"})
    );
    assert!(backend.calls().is_empty());
}

#[test]
fn import_of_bytes_stages_them_as_a_fantome_and_cleans_up() {
    use base64::Engine;
    let (bridge, backend) = bridge(FakeBackend::default());
    let data = base64::engine::general_purpose::STANDARD.encode(b"archive");

    let answer = send(
        &bridge,
        authed(json!({"cmd": "import", "data_b64": data, "name": "../Lux Skin"})),
    );

    assert_eq!(answer["ok"], json!(true));
    // The staged file existed while it was installed.
    assert_eq!(answer["response"]["value"]["staged"], json!(true));
    let calls = backend.calls();
    let staged = calls[0].strip_prefix("install ").unwrap();
    assert!(staged.ends_with("Lux Skin.fantome"), "{staged}");
    assert!(!std::path::Path::new(staged).exists());
}

#[test]
fn import_of_bad_base64_is_refused() {
    let (bridge, _) = bridge(FakeBackend::default());

    let answer = send(&bridge, authed(json!({"cmd": "import", "data_b64": "%%%"})));

    assert_eq!(
        answer,
        json!({"ok": false, "error": "invalid base64 in 'data_b64'"})
    );
}

#[test]
fn import_without_a_source_is_refused() {
    let (bridge, _) = bridge(FakeBackend::default());

    let answer = send(&bridge, authed(json!({"cmd": "import"})));

    assert_eq!(
        answer,
        json!({"ok": false, "error": "missing 'path' or 'data_b64'"})
    );
}

#[test]
fn a_patcher_error_keeps_its_kind_inside_the_response() {
    let (bridge, _) = bridge(FakeBackend {
        patcher_running: true,
        ..FakeBackend::default()
    });

    let answer = send(&bridge, authed(json!({"cmd": "start_patcher"})));

    assert_eq!(answer["ok"], json!(false));
    assert_eq!(
        answer["response"]["error"]["error"]["kind"],
        json!("ALREADY_RUNNING")
    );
}

#[test]
fn patcher_status_wraps_the_envelope() {
    let (bridge, _) = bridge(FakeBackend::default());

    let answer = send(&bridge, authed(json!({"cmd": "patcher_status"})));

    assert_eq!(answer["ok"], json!(true));
    assert_eq!(answer["response"]["value"]["running"], json!(true));
}

#[test]
fn mod_health_commands_need_a_mod_id() {
    let (bridge, backend) = bridge(FakeBackend::default());

    for cmd in ["check_mod_health", "repair_mod"] {
        let answer = send(&bridge, authed(json!({"cmd": cmd})));
        assert_eq!(answer, json!({"ok": false, "error": "missing 'modId'"}));
    }
    send(
        &bridge,
        authed(json!({"cmd": "check_mod_health", "modId": "a1"})),
    );
    assert_eq!(backend.calls(), vec!["check a1"]);
}

#[test]
fn health_check_off_reports_nothing_changed() {
    let (bridge, _) = bridge(FakeBackend::default());

    let answer = send(&bridge, authed(json!({"cmd": "health_check_off"})));

    assert_eq!(answer, json!({"ok": true, "changed": false}));
}

#[test]
fn kill_asks_the_app_to_quit() {
    let (bridge, backend) = bridge(FakeBackend::default());

    let answer = send(&bridge, authed(json!({"cmd": "kill"})));

    assert_eq!(answer["ok"], json!(true));
    assert_eq!(backend.calls(), vec!["quit"]);
}

#[tokio::test]
async fn the_listener_takes_the_next_free_port_in_the_range() {
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let busy = taken.local_addr().unwrap().port();

    let (_listener, port) = bind_listener(busy..=busy.saturating_add(5)).await.unwrap();

    assert_ne!(port, busy);
    assert!(port > busy && port <= busy.saturating_add(5));
}

#[test]
fn the_port_and_token_files_are_written_for_the_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bridge_dir = dir.path().join("LTKBridge");

    publish_port_and_token(&bridge_dir, 46325, TOKEN).unwrap();

    assert_eq!(
        fs_err::read_to_string(bridge_dir.join("port.txt")).unwrap(),
        "46325\n"
    );
    assert_eq!(
        fs_err::read_to_string(bridge_dir.join("token.txt")).unwrap(),
        format!("{TOKEN}\n")
    );
    let mode = fs_err::metadata(bridge_dir.join("token.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
