use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::super::{
    DecorationCachedEvaluationResult, DecorationEvaluationError, DecorationEvaluationResult,
    DecorationEvaluator, DecorationHandlerInvocation, DecorationTree, ManagedWindowState,
    WaylandOutputSnapshot, WaylandWindowSnapshot, WindowTransform,
};
use super::{
    protocol::{ExternalRuntimeRequest, ExternalRuntimeResponse},
    transport::{ExternalTransport, RESPONSE_TIMEOUT},
};
use crate::runtime_input::RuntimeInputDeviceSnapshot;

/// Parallel implementation of the existing evaluator boundary. The embedded
/// runtime and all its native bridge entry points remain independent.
#[derive(Debug, Clone)]
pub struct DotNetDecorationEvaluator {
    executable: PathBuf,
    config: PathBuf,
    state: Arc<Mutex<RuntimeState>>,
    pending: Option<Arc<PendingAssembly>>,
    // Drops after pending cleanup and the state/worker, so config dependencies
    // remain available during abort/dispose as well as ordinary shutdown.
    generation: Option<Arc<super::assembly::GenerationDirectory>>,
}

/// A prepared candidate must be aborted when validation, a newer save or event
/// delivery cancels it. The lease holds no old generation/assembly directory.
#[derive(Debug)]
struct PendingAssembly {
    state: Arc<Mutex<RuntimeState>>,
    completed: AtomicBool,
}

impl Drop for PendingAssembly {
    fn drop(&mut self) {
        if !self.completed.load(Ordering::Acquire) {
            let owner = DotNetDecorationEvaluator {
                executable: PathBuf::new(),
                config: PathBuf::new(),
                state: self.state.clone(),
                generation: None,
                pending: None,
            };
            if let Ok(mut state) = owner.lock() {
                if let Err(error) =
                    owner.request(&mut state, "abortAssembly", None, None, None, 0, None)
                {
                    tracing::warn!(%error, "failed to abort prepared C# assembly");
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct RuntimeState {
    transport: Option<ExternalTransport>,
    next_request_id: u64,
    failure: Option<String>,
    displays: BTreeMap<String, WaylandOutputSnapshot>,
    inputs: BTreeMap<String, RuntimeInputDeviceSnapshot>,
    windows: BTreeMap<String, (WaylandWindowSnapshot, DecorationEvaluationResult)>,
}

impl RuntimeState {
    fn quarantine(&mut self, error: String) {
        if let Some(mut transport) = self.transport.take() {
            transport.stop();
        }
        self.failure = Some(error);
    }
}

impl Drop for RuntimeState {
    fn drop(&mut self) {
        // Give a healthy config a bounded opportunity to disable before the
        // transport kills/reaps its worker. A wedged config cannot delay exit.
        if let Some(transport) = self.transport.as_mut()
            && let Some(request_id) = self.next_request_id.checked_add(1)
        {
            let request = ExternalRuntimeRequest {
                request_id,
                kind: "shutdownAssemblies",
                snapshot: None,
                window_id: None,
                handler_id: None,
                now_ms: 0,
                reason: Some("shutdown"),
                config_path: None,
                display_state: &self.displays,
                input_state: &self.inputs,
            };
            if let Ok(bytes) = serde_json::to_vec(&request) {
                let _ = transport.exchange(bytes, std::time::Duration::from_millis(100));
            }
        }
    }
}

impl DotNetDecorationEvaluator {
    #[cfg(test)]
    pub(super) fn test_process_id(&self) -> Option<u32> {
        self.state
            .lock()
            .unwrap()
            .transport
            .as_ref()
            .map(ExternalTransport::process_id)
    }

    #[cfg(test)]
    pub(super) fn test_generation_path(&self) -> Option<PathBuf> {
        self.generation
            .as_ref()
            .map(|directory| directory.path().to_path_buf())
    }
    pub fn new(executable: PathBuf, config: PathBuf) -> Self {
        Self {
            executable,
            config,
            state: Arc::new(Mutex::new(RuntimeState::default())),
            generation: None,
            pending: None,
        }
    }

    pub fn new_shadowed(executable: PathBuf, config: PathBuf) -> Self {
        match super::assembly::GenerationDirectory::copy_config(&config) {
            Ok((directory, staged)) => Self::for_generation(executable, staged, directory),
            Err(error) => {
                let evaluator = Self::new(executable, config);
                if let Ok(mut state) = evaluator.state.lock() {
                    state.failure = Some(error);
                }
                evaluator
            }
        }
    }

    pub(super) fn for_generation(
        executable: PathBuf,
        config: PathBuf,
        directory: Arc<super::assembly::GenerationDirectory>,
    ) -> Self {
        let mut evaluator = Self::new(executable, config);
        evaluator.generation = Some(directory);
        evaluator
    }

    pub(super) fn has_active_worker(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.failure.is_none() && state.transport.is_some())
    }

    pub(crate) fn shares_worker_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub(super) fn prepare_assembly(
        &self,
        config: PathBuf,
        directory: Arc<super::assembly::GenerationDirectory>,
    ) -> Result<Self, DecorationEvaluationError> {
        let mut next = self.clone();
        next.config = config;
        next.generation = Some(directory);
        next.pending = Some(Arc::new(PendingAssembly {
            state: self.state.clone(),
            completed: AtomicBool::new(false),
        }));
        {
            let mut state = self.lock()?;
            let path = next.config.to_str().ok_or_else(|| {
                DecorationEvaluationError::RuntimeProtocol("config path must be UTF-8".into())
            })?;
            self.request_with_config(
                &mut state,
                "prepareAssembly",
                None,
                None,
                None,
                0,
                None,
                Some(path),
            )?;
        }
        Ok(next)
    }

    /// Only the existing .NET integration calls this at the compositor commit
    /// boundary. Ordinary reload retains the worker; crash recovery replaces it.
    pub(crate) fn activate_prepared(&self) -> Result<(), DecorationEvaluationError> {
        if let Some(pending) = &self.pending {
            if pending.completed.load(Ordering::Acquire) {
                return Ok(());
            }
            let mut state = self.lock()?;
            self.request(&mut state, "commitAssembly", None, None, None, 0, None)?;
            state.windows.clear();
            pending.completed.store(true, Ordering::Release);
        }
        Ok(())
    }

    pub(super) fn validate_candidate(
        &self,
        snapshot: &WaylandWindowSnapshot,
    ) -> Result<(), DecorationEvaluationError> {
        let kind = if self.pending.is_some() {
            "evaluateCandidatePreview"
        } else {
            "evaluatePreview"
        };
        self.render(snapshot, 0, kind).map(|_| ())
    }

    pub(crate) fn copy_environment_to(&self, next: &Self) -> Result<(), DecorationEvaluationError> {
        if self.shares_worker_with(next) {
            return Ok(());
        }
        let state = self.lock()?;
        next.set_display_state(state.displays.clone());
        next.set_input_state(state.inputs.clone());
        Ok(())
    }

    pub(super) fn window_snapshots(
        &self,
    ) -> Result<Vec<WaylandWindowSnapshot>, DecorationEvaluationError> {
        Ok(self
            .lock()?
            .windows
            .values()
            .map(|(snapshot, _)| snapshot.clone())
            .collect())
    }

    /// Retire all evaluator clones at commit, with bounded cleanup even when
    /// user OnDisable throws or stalls. Process death clears timers/statics.
    pub(crate) fn retire(&self, reason: &str) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(mut transport) = state.transport.take() {
                let request = ExternalRuntimeRequest {
                    request_id: state.next_request_id.saturating_add(1),
                    kind: "shutdownAssemblies",
                    snapshot: None,
                    window_id: None,
                    handler_id: None,
                    now_ms: 0,
                    reason: Some(reason),
                    config_path: None,
                    display_state: &state.displays,
                    input_state: &state.inputs,
                };
                if let Ok(bytes) = serde_json::to_vec(&request) {
                    match transport.exchange(bytes, std::time::Duration::from_millis(100)) {
                        Ok(bytes) => {
                            match serde_json::from_slice::<ExternalRuntimeResponse>(&bytes) {
                                Ok(response) if response.ok => {}
                                Ok(response) => {
                                    tracing::warn!(error = ?response.error, "C# disable callback failed; terminating worker")
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "invalid C# disable response; terminating worker")
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "C# generation cleanup failed; terminating worker")
                        }
                    }
                }
                transport.stop();
            }
            state.windows.clear();
            state.failure = Some("runtime generation retired".into());
        }
    }

    pub fn set_display_state(&self, displays: BTreeMap<String, WaylandOutputSnapshot>) {
        if let Ok(mut state) = self.state.lock() {
            state.displays = displays;
        }
    }

    pub fn set_input_state(&self, inputs: BTreeMap<String, RuntimeInputDeviceSnapshot>) {
        if let Ok(mut state) = self.state.lock() {
            state.inputs = inputs;
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, RuntimeState>, DecorationEvaluationError> {
        self.state.lock().map_err(|_| {
            DecorationEvaluationError::RuntimeProtocol(".NET runtime mutex poisoned".into())
        })
    }

    fn request(
        &self,
        state: &mut RuntimeState,
        kind: &str,
        snapshot: Option<&WaylandWindowSnapshot>,
        window_id: Option<&str>,
        handler_id: Option<&str>,
        now_ms: u64,
        reason: Option<&str>,
    ) -> Result<ExternalRuntimeResponse, DecorationEvaluationError> {
        self.request_with_config(
            state, kind, snapshot, window_id, handler_id, now_ms, reason, None,
        )
    }

    fn request_with_config(
        &self,
        state: &mut RuntimeState,
        kind: &str,
        snapshot: Option<&WaylandWindowSnapshot>,
        window_id: Option<&str>,
        handler_id: Option<&str>,
        now_ms: u64,
        reason: Option<&str>,
        config_path: Option<&str>,
    ) -> Result<ExternalRuntimeResponse, DecorationEvaluationError> {
        if let Some(error) = &state.failure {
            return Err(DecorationEvaluationError::RuntimeProtocol(error.clone()));
        }
        let mut rejected_candidate = false;
        let result = (|| -> Result<ExternalRuntimeResponse, String> {
            if state.transport.is_none() {
                state.transport = Some(ExternalTransport::start(&self.executable, &self.config)?);
            }
            state.next_request_id = state
                .next_request_id
                .checked_add(1)
                .ok_or("requestId exhausted")?;
            let request_id = state.next_request_id;
            let request = ExternalRuntimeRequest {
                request_id,
                kind,
                snapshot,
                window_id,
                handler_id,
                now_ms,
                reason,
                config_path,
                display_state: &state.displays,
                input_state: &state.inputs,
            };
            let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
            let bytes = state
                .transport
                .as_mut()
                .ok_or("external transport unavailable")?
                .exchange(bytes, RESPONSE_TIMEOUT)?;
            let response: ExternalRuntimeResponse = serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid external runtime response: {e}"))?;
            if response.request_id != request_id || response.kind != kind {
                return Err(format!(
                    "mismatched response: expected {kind}/{request_id}, got {}/{}",
                    response.kind, response.request_id
                ));
            }
            if !response.ok {
                rejected_candidate = matches!(
                    kind,
                    "prepareAssembly"
                        | "evaluateCandidatePreview"
                        | "abortAssembly"
                        | "commitAssembly"
                );
                return Err(response
                    .error
                    .unwrap_or_else(|| "external runtime returned failure".into()));
            }
            Ok(response)
        })();
        if let Err(error) = &result
            && !rejected_candidate
        {
            state.quarantine(error.clone());
        }
        result.map_err(DecorationEvaluationError::RuntimeProtocol)
    }

    pub fn preload(&self) -> Result<(), DecorationEvaluationError> {
        let mut state = self.lock()?;
        self.request(&mut state, "drainPreload", None, None, None, 0, None)?;
        Ok(())
    }

    pub fn lifecycle_enable(
        &self,
        reason: &str,
    ) -> Result<DecorationHandlerInvocation, DecorationEvaluationError> {
        let mut state = self.lock()?;
        let response = self.request(
            &mut state,
            "lifecycleEnable",
            None,
            None,
            None,
            0,
            Some(reason),
        )?;
        Ok(DecorationHandlerInvocation {
            actions: response.actions,
            ..Default::default()
        })
    }

    fn render(
        &self,
        snapshot: &WaylandWindowSnapshot,
        now_ms: u64,
        kind: &str,
    ) -> Result<DecorationEvaluationResult, DecorationEvaluationError> {
        let mut state = self.lock()?;
        let response = self.request(
            &mut state,
            kind,
            Some(snapshot),
            Some(&snapshot.id),
            None,
            now_ms,
            None,
        )?;
        let node = Self::decode_response_tree(
            &mut state,
            response.serialized,
            true,
            kind != "evaluateCandidatePreview",
        )?
        .ok_or_else(|| {
            DecorationEvaluationError::RuntimeProtocol("missing composition tree".into())
        })?;
        let result = DecorationEvaluationResult {
            node,
            transform: WindowTransform::default(),
            managed_window: ManagedWindowState::default(),
            window_effects: None,
            dirty_node_ids: Vec::new(),
            next_poll_in_ms: None,
            actions: response.actions,
            display_config: None,
            workspace_config: None,
            key_binding_config: None,
            pointer_config: None,
            input_config: None,
            event_config: None,
            process_config: None,
            process_actions: Vec::new(),
        };
        if !matches!(kind, "evaluatePreview" | "evaluateCandidatePreview") {
            state
                .windows
                .insert(snapshot.id.clone(), (snapshot.clone(), result.clone()));
        }
        Ok(result)
    }

    fn decode_response_tree(
        state: &mut RuntimeState,
        wire: Option<super::super::WireDecorationNode>,
        required: bool,
        quarantine: bool,
    ) -> Result<Option<super::super::DecorationNode>, DecorationEvaluationError> {
        let result = (|| {
            let Some(wire) = wire else {
                return if required {
                    Err(DecorationEvaluationError::RuntimeProtocol(
                        "missing serialized composition tree".into(),
                    ))
                } else {
                    Ok(None)
                };
            };
            // Exactly the same conversion/structural validation as the TS wire path.
            let node: super::super::DecorationNode = wire.try_into()?;
            DecorationTree::new(node.clone())
                .validate()
                .map_err(|error| {
                    DecorationEvaluationError::RuntimeProtocol(format!(
                        "invalid composition tree: {error:?}"
                    ))
                })?;
            Ok(Some(node))
        })();
        if let Err(error) = &result
            && quarantine
        {
            state.quarantine(error.to_string());
        }
        result
    }
}

impl DecorationEvaluator for DotNetDecorationEvaluator {
    fn evaluate_window(
        &self,
        snapshot: &WaylandWindowSnapshot,
        now_ms: u64,
    ) -> Result<DecorationEvaluationResult, DecorationEvaluationError> {
        self.render(snapshot, now_ms, "evaluate")
    }

    fn evaluate_window_preview(
        &self,
        snapshot: &WaylandWindowSnapshot,
        now_ms: u64,
    ) -> Result<DecorationEvaluationResult, DecorationEvaluationError> {
        self.render(snapshot, now_ms, "evaluatePreview")
    }

    fn evaluate_cached_window(
        &self,
        window_id: &str,
        snapshot: Option<&WaylandWindowSnapshot>,
        now_ms: u64,
        force_full: bool,
    ) -> Result<DecorationCachedEvaluationResult, DecorationEvaluationError> {
        if let Some(snapshot) = snapshot {
            if snapshot.id != window_id {
                return Err(DecorationEvaluationError::RuntimeProtocol(
                    "cached snapshot windowId mismatch".into(),
                ));
            }
            return self
                .render(snapshot, now_ms, "evaluateCached")
                .map(Into::into);
        }
        let state = self.lock()?;
        if let Some(error) = &state.failure {
            return Err(DecorationEvaluationError::RuntimeProtocol(error.clone()));
        }
        let (snapshot, result) = state.windows.get(window_id).cloned().ok_or_else(|| {
            DecorationEvaluationError::RuntimeProtocol(format!(
                "unknown cached window: {window_id}"
            ))
        })?;
        drop(state);
        if force_full {
            return self
                .render(&snapshot, now_ms, "evaluateCached")
                .map(Into::into);
        }
        let mut cached: DecorationCachedEvaluationResult = result.into();
        cached.node = None;
        cached.actions.clear();
        Ok(cached)
    }

    fn invoke_handler(
        &self,
        window_id: &str,
        handler_id: &str,
        now_ms: u64,
    ) -> Result<DecorationHandlerInvocation, DecorationEvaluationError> {
        let mut state = self.lock()?;
        let response = self.request(
            &mut state,
            "invokeHandler",
            None,
            Some(window_id),
            Some(handler_id),
            now_ms,
            None,
        )?;
        let node = Self::decode_response_tree(&mut state, response.serialized, false, true)?;
        if let Some(node) = &node {
            if let Some((_, cached)) = state.windows.get_mut(window_id) {
                cached.node = node.clone();
            }
        }
        Ok(DecorationHandlerInvocation {
            invoked: response.invoked.unwrap_or(false),
            node,
            actions: response.actions,
            ..Default::default()
        })
    }

    fn window_closed(&self, window_id: &str) -> Result<(), DecorationEvaluationError> {
        let mut state = self.lock()?;
        state.windows.remove(window_id);
        self.request(
            &mut state,
            "windowClosed",
            None,
            Some(window_id),
            None,
            0,
            None,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{
        DecorationNodeKind, WindowAction, window_model::WindowPositionSnapshot,
    };
    use super::*;
    use std::process::Command;

    fn snapshot() -> WaylandWindowSnapshot {
        let rect = WindowPositionSnapshot {
            x: 0.5,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        WaylandWindowSnapshot {
            id: "1".into(),
            title: "Kitty 日本語".into(),
            app_id: Some("kitty".into()),
            position: rect,
            rect,
            is_focused: true,
            is_floating: true,
            is_maximized: false,
            is_fullscreen: false,
            is_xwayland: false,
            decoration: Default::default(),
            size_constraints: Default::default(),
            is_resizable: true,
            is_transient: false,
            parent_id: None,
            icon: None,
            interaction: Default::default(),
        }
    }

    fn fake_worker(mode: &str) -> DotNetDecorationEvaluator {
        let script = r#"
import sys, json
mode = sys.argv[1]
for line in sys.stdin:
    request = json.loads(line)
    response = dict(requestId=request['requestId'], kind=request['kind'], ok=True)
    if 'snapshot' in request:
        response['serialized'] = dict(kind='WindowBorder', children=[
            dict(kind='Label', props=dict(text=request['snapshot']['title'])), dict(kind='Window')])
    if mode == 'id': response['requestId'] += 1
    if mode == 'kind': response['kind'] = 'unknown'
    if mode == 'missing': response.pop('serialized', None)
    if mode == 'invalid': response['serialized'] = dict(kind='Box')
    if mode == 'unsupported': response['serialized'] = dict(kind='Unknown')
    if mode == 'json': print('bad json', flush=True)
    else: print(json.dumps(response), flush=True)
"#;
        let mut command = Command::new("python3");
        command.arg("-u").arg("-c").arg(script).arg(mode);
        let evaluator = DotNetDecorationEvaluator::new(PathBuf::new(), PathBuf::new());
        evaluator.state.lock().unwrap().transport =
            Some(ExternalTransport::spawn(command).unwrap());
        evaluator
    }

    #[test]
    fn shared_snapshot_fixture_matches_actual_rust_wire() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../../../dotnet/fixtures/window.json"))
                .unwrap();
        assert_eq!(serde_json::to_value(snapshot()).unwrap(), fixture);
    }

    #[test]
    fn fake_worker_snapshot_to_existing_decoder_and_cache() {
        let evaluator = fake_worker("ok");
        evaluator.preload().unwrap();
        let result = evaluator.evaluate_window(&snapshot(), 1234).unwrap();
        DecorationTree::new(result.node.clone()).validate().unwrap();
        let DecorationNodeKind::Label(label) = &result.node.children[0].kind else {
            panic!("missing label");
        };
        assert_eq!(label.text, "Kitty 日本語");
        assert!(
            evaluator
                .evaluate_cached_window("1", None, 1235, false)
                .unwrap()
                .node
                .is_none()
        );
        assert!(
            evaluator
                .evaluate_cached_window("1", None, 1236, true)
                .unwrap()
                .node
                .is_some()
        );
        evaluator.window_closed("1").unwrap();
        assert!(
            evaluator
                .evaluate_cached_window("1", None, 1237, false)
                .is_err()
        );
    }

    #[test]
    fn malformed_and_mismatched_responses_are_errors() {
        for mode in ["id", "kind", "json", "missing", "invalid", "unsupported"] {
            let evaluator = fake_worker(mode);
            assert!(
                evaluator.evaluate_window(&snapshot(), 1).is_err(),
                "accepted {mode} response"
            );
            assert!(evaluator.state.lock().unwrap().transport.is_none());
        }
    }

    #[test]
    fn protocol_failure_quarantines_worker_without_respawn() {
        let evaluator = fake_worker("id");
        assert!(evaluator.preload().is_err());
        assert!(evaluator.state.lock().unwrap().transport.is_none());
        assert!(
            evaluator
                .preload()
                .unwrap_err()
                .to_string()
                .contains("mismatched response")
        );
    }

    fn handler_id(node: &super::super::super::DecorationNode) -> Option<&str> {
        if let DecorationNodeKind::Button(button) = &node.kind
            && let WindowAction::RuntimeHandler(id) = &button.action
        {
            return Some(id);
        }
        node.children.iter().find_map(handler_id)
    }

    #[test]
    #[ignore = "build .NET projects and set SHOJI_TEST_DOTNET_RUNTIME / SHOJI_TEST_DOTNET_CONFIG"]
    fn real_dotnet_worker_decodes_example_and_dispatches_delegate() {
        let evaluator = DotNetDecorationEvaluator::new(
            std::env::var_os("SHOJI_TEST_DOTNET_RUNTIME")
                .expect("runtime apphost path")
                .into(),
            std::env::var_os("SHOJI_TEST_DOTNET_CONFIG")
                .expect("example assembly path")
                .into(),
        );
        evaluator.preload().unwrap();
        evaluator.lifecycle_enable("initial").unwrap();
        let result = evaluator.evaluate_window(&snapshot(), 1234).unwrap();
        let tree = DecorationTree::new(result.node.clone());
        tree.validate().unwrap();
        let layout = tree
            .layout(super::super::super::LogicalRect::new(0, 0, 802, 630))
            .unwrap();
        assert!(layout.window_slot_rect().is_some());
        assert!(!layout.render_primitives().is_empty());
        let handler = handler_id(&result.node).expect("close handler").to_string();
        let invoked = evaluator.invoke_handler("1", &handler, 1235).unwrap();
        assert!(invoked.invoked && invoked.node.is_some());
        assert_eq!(invoked.actions.len(), 1);
        assert_eq!(invoked.actions[0].window_id, "1");
        assert_eq!(
            invoked.actions[0].action,
            super::super::super::WaylandWindowAction::Close
        );
        evaluator.window_closed("1").unwrap();
        assert!(
            !evaluator
                .invoke_handler("1", &handler, 1236)
                .unwrap()
                .invoked
        );
    }
}
