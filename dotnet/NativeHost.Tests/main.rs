#[path = "../../src/shojiwm/src/ssd/dotnet/host.rs"]
mod host;
use host::InProcessDotNetHost;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

fn request(
    host: &Arc<Mutex<InProcessDotNetHost>>,
    kind: &str,
    id: u64,
    config: Option<&PathBuf>,
    handler: Option<&str>,
) -> Value {
    let snapshot: Value = serde_json::from_str(include_str!("../fixtures/window.json")).unwrap();
    let bytes = serde_json::to_vec(&json!({"kind":kind, "requestId":id, "nowMs":1234,
        "displayState":{}, "inputState":{}, "snapshot":snapshot, "windowId":"1",
        "configPath":config, "handlerId":handler, "reason":"initial"}))
    .unwrap();
    let response = host.lock().unwrap().exchange(bytes).unwrap();
    let value: Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(value["requestId"], id);
    assert_eq!(value["kind"], kind);
    value
}
fn handler(tree: &Value) -> Option<&str> {
    if tree["kind"] == "Button" {
        return tree["props"]["onClick"]["id"].as_str();
    }
    tree["children"].as_array()?.iter().find_map(handler)
}
fn env(name: &str, value: &str) {
    // Test fixture environment is changed only while no managed callback is
    // executing; fixture backgrounds do not read these variables.
    unsafe {
        std::env::set_var(name, value);
    }
}
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let runtime = PathBuf::from(&args[1]);
    let original = PathBuf::from(&args[2]);
    let root = std::env::temp_dir().join(format!(
        "shoji-native-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let staging = root.join("config");
    std::fs::create_dir(&staging).unwrap();
    for entry in std::fs::read_dir(original.parent().unwrap()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), staging.join(entry.file_name())).unwrap();
        }
    }
    let fixture = staging.join(original.file_name().unwrap());
    let settings = |text: &str, failure: &str| {
        std::fs::write(
            staging.join("fixture-settings.json"),
            serde_json::to_vec(&json!({"text":text,"failure":failure})).unwrap(),
        )
        .unwrap()
    };
    settings("initial", "");
    let threads = root.join("threads.log");
    let disposal = root.join("disposal.log");
    env("SHOJI_TEST_THREAD_MARKER", threads.to_str().unwrap());
    env("SHOJI_TEST_CONFIG_MARKER", disposal.to_str().unwrap());
    settings("initial", "");
    assert!(InProcessDotNetHost::start(&runtime, &root.join("missing.dll")).is_err());
    let host = Arc::new(Mutex::new(
        InProcessDotNetHost::start(&runtime.with_extension(""), &fixture).unwrap(),
    ));
    assert!(request(&host, "lifecycleEnable", 1, None, None)["ok"] == true);
    let first = request(&host, "evaluate", 2, None, None);
    let mut previous = handler(&first["serialized"]).unwrap().to_owned();
    let mut ids = HashSet::from([previous.clone()]);
    for generation in 0..20 {
        settings(&format!("generation-{generation}"), "");
        let clone = host.clone();
        let path = fixture.clone();
        // Reload called from a different Rust thread still enters the same
        // managed host thread. No native delegate points into the config ALC.
        std::thread::spawn(move || {
            assert!(request(&clone, "prepareAssembly", 10, Some(&path), None)["ok"] == true);
            assert!(request(&clone, "evaluateCandidatePreview", 11, None, None)["ok"] == true);
            assert!(request(&clone, "commitAssembly", 12, None, None)["ok"] == true);
        })
        .join()
        .unwrap();
        assert!(request(&host, "invokeHandler", 13, None, Some(&previous))["invoked"] == false);
        let tree = request(&host, "evaluate", 14, None, None);
        assert_eq!(
            tree["serialized"]["children"][0]["props"]["text"],
            format!("generation-{generation}")
        );
        previous = handler(&tree["serialized"]).unwrap().into();
        assert!(ids.insert(previous.clone()));
        let action = request(&host, "invokeHandler", 15, None, Some(&previous));
        assert!(action["invoked"] == true);
        assert_eq!(action["actions"][0]["action"], "close");
    }
    // Size guards replace pipe-line limits, including oversized managed output.
    settings(&"x".repeat(host::MAX_MESSAGE_BYTES + 1), "");
    assert!(request(&host, "prepareAssembly", 16, Some(&fixture), None)["ok"] == true);
    let snapshot: Value = serde_json::from_str(include_str!("../fixtures/window.json")).unwrap();
    let preview = serde_json::to_vec(&json!({"kind":"evaluateCandidatePreview","requestId":17,"snapshot":snapshot,"windowId":"1","nowMs":0,"displayState":{},"inputState":{}})).unwrap();
    let oversized: Value =
        serde_json::from_slice(&host.lock().unwrap().exchange(preview).unwrap()).unwrap();
    assert!(oversized["ok"] == false);
    assert_eq!(oversized["requestId"], 17);
    assert!(oversized["error"].as_str().unwrap().contains("8 MiB"));
    assert!(request(&host, "abortAssembly", 18, None, None)["ok"] == true);
    for failure in ["constructor", "enable", "render"] {
        settings("candidate", failure);
        let prepare = request(&host, "prepareAssembly", 20, Some(&fixture), None);
        if failure == "render" {
            assert!(prepare["ok"] == true);
            assert!(request(&host, "evaluateCandidatePreview", 21, None, None)["ok"] == false);
            assert!(request(&host, "abortAssembly", 22, None, None)["ok"] == true);
        } else {
            assert!(prepare["ok"] == false);
        }
        assert!(request(&host, "evaluate", 23, None, None)["ok"] == true);
    }
    settings("accepted", "");
    let broken = root.join("broken.dll");
    std::fs::write(&broken, b"broken bytes").unwrap();
    for path in [broken, root.join("missing.dll"), runtime.clone()] {
        assert!(request(&host, "prepareAssembly", 25, Some(&path), None)["ok"] == false);
        assert!(request(&host, "evaluate", 26, None, None)["ok"] == true);
    }
    settings("leaked", "leak");
    assert!(request(&host, "prepareAssembly", 30, Some(&fixture), None)["ok"] == true);
    assert!(request(&host, "commitAssembly", 31, None, None)["ok"] == true);
    settings("accepted", "");
    assert!(request(&host, "prepareAssembly", 32, Some(&fixture), None)["ok"] == true);
    assert!(request(&host, "commitAssembly", 33, None, None)["ok"] == true);
    let refusal = request(&host, "prepareAssembly", 34, Some(&fixture), None);
    assert!(refusal["ok"] == false);
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("still referenced")
    );
    let tree = request(&host, "evaluate", 35, None, None);
    let id = handler(&tree["serialized"]).unwrap();
    assert!(request(&host, "invokeHandler", 36, None, Some(id))["ok"] == true);
    assert!(request(&host, "prepareAssembly", 37, Some(&fixture), None)["ok"] == true);
    assert!(request(&host, "abortAssembly", 38, None, None)["ok"] == true);
    assert!(
        host.lock()
            .unwrap()
            .exchange(vec![b'x'; host::MAX_MESSAGE_BYTES + 1])
            .is_err()
    );
    let malformed = host.lock().unwrap().exchange(b"bad json".to_vec()).unwrap();
    assert!(serde_json::from_slice::<Value>(&malformed).unwrap()["ok"] == false);
    assert!(request(&host, "shutdownAssemblies", 40, None, None)["ok"] == true);
    drop(host);
    let trace = std::fs::read_to_string(threads).unwrap();
    let lines: Vec<_> = trace.lines().collect();
    let thread_ids: HashSet<_> = lines
        .iter()
        .map(|line| line.split(':').nth(1).unwrap())
        .collect();
    assert_eq!(
        thread_ids.len(),
        1,
        "callbacks crossed managed threads: {trace}"
    );
    for operation in [
        "constructor",
        "enable",
        "render",
        "handler",
        "disable",
        "dispose",
    ] {
        assert!(lines.iter().any(|line| line.starts_with(operation)));
    }
    let disposed = std::fs::read_to_string(disposal).unwrap();
    assert!(
        disposed
            .lines()
            .filter(|line| line.starts_with("dispose:"))
            .count()
            >= 27
    );
    // No worker is launched by production hosting (including from CLR threads).
    for entry in std::fs::read_dir("/proc/self/task").unwrap() {
        let children = std::fs::read_to_string(entry.unwrap().path().join("children"));
        if let Ok(children) = children {
            assert!(
                children.trim().is_empty(),
                "hosting spawned a child: {children}"
            );
        }
    }
    std::fs::remove_dir_all(root).unwrap();
    println!(
        "PASS hostfxr bootstrap, API identity, UTF-8 JSON, handlers/actions, 20 reloads, rollback, leak/refusal/retry, buffers and single managed thread"
    );
}
