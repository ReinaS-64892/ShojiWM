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
    host::InProcessDotNetHost,
    protocol::{ExternalRuntimeRequest, ExternalRuntimeResponse},
};
use crate::runtime_input::RuntimeInputDeviceSnapshot;

/// Parallel implementation of the existing evaluator boundary. The embedded
/// runtime and all its native bridge entry points remain independent.
#[derive(Debug, Clone)]
pub struct DotNetDecorationEvaluator {
    component: PathBuf,
    config: PathBuf,
    state: Arc<Mutex<RuntimeState>>,
    pending: Option<Arc<PendingAssembly>>,
    // Drops after pending cleanup and the state/host, so config dependencies
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
                component: PathBuf::new(),
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
    host: Option<InProcessDotNetHost>,
    next_request_id: u64,
    failure: Option<String>,
    displays: BTreeMap<String, WaylandOutputSnapshot>,
    inputs: BTreeMap<String, RuntimeInputDeviceSnapshot>,
    windows: BTreeMap<String, (WaylandWindowSnapshot, DecorationEvaluationResult)>,
}

impl RuntimeState {
    fn fail_host(&mut self, error: String) {
        self.failure = Some(format!("{error}; ShojiWM restart required"));
    }
}

// InProcessDotNetHost owns RAII destruction on the managed host thread.
// CLR/bootstrap remain loaded for process lifetime.

impl DotNetDecorationEvaluator {
    #[cfg(test)]
    pub(super) fn test_generation_path(&self) -> Option<PathBuf> {
        self.generation
            .as_ref()
            .map(|directory| directory.path().to_path_buf())
    }
    pub fn new(component: PathBuf, config: PathBuf) -> Self {
        Self {
            component,
            config,
            state: Arc::new(Mutex::new(RuntimeState::default())),
            generation: None,
            pending: None,
        }
    }

    pub fn new_shadowed(component: PathBuf, config: PathBuf) -> Self {
        match super::assembly::GenerationDirectory::copy_config(&config) {
            Ok((directory, staged)) => Self::for_generation(component, staged, directory),
            Err(error) => {
                let evaluator = Self::new(component, config);
                if let Ok(mut state) = evaluator.state.lock() {
                    state.failure = Some(error);
                }
                evaluator
            }
        }
    }

    pub(super) fn for_generation(
        component: PathBuf,
        config: PathBuf,
        directory: Arc<super::assembly::GenerationDirectory>,
    ) -> Self {
        let mut evaluator = Self::new(component, config);
        evaluator.generation = Some(directory);
        evaluator
    }

    pub(super) fn has_active_host(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.failure.is_none() && state.host.is_some())
    }

    pub(crate) fn shares_host_with(&self, other: &Self) -> bool {
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
    /// boundary. Ordinary reload retains the managed host and permanent bootstrap.
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
        if self.shares_host_with(next) {
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

    /// Dispose the managed host on its original thread. User cleanup can block;
    /// there is deliberately no unsafe thread termination or CLR restart.
    pub(crate) fn retire(&self, _reason: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.host.take();
            state.windows.clear();
            state.failure = Some("runtime host retired; ShojiWM restart required".into());
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
        let mut managed_failure = false;
        let result = (|| -> Result<ExternalRuntimeResponse, String> {
            if state.host.is_none() {
                state.host = Some(InProcessDotNetHost::start(&self.component, &self.config)?);
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
                .host
                .as_mut()
                .ok_or("runtime host unavailable")?
                .exchange(bytes)?;
            let response = Self::decode_response(&bytes, kind, request_id)?;
            if !response.ok {
                managed_failure = true;
                return Err(response
                    .error
                    .unwrap_or_else(|| "managed runtime returned failure".into()));
            }
            Ok(response)
        })();
        if let Err(error) = &result
            && !managed_failure
        {
            state.fail_host(error.clone());
        }
        result.map_err(DecorationEvaluationError::RuntimeProtocol)
    }

    fn decode_response(
        bytes: &[u8],
        kind: &str,
        request_id: u64,
    ) -> Result<ExternalRuntimeResponse, String> {
        let response: ExternalRuntimeResponse = serde_json::from_slice(bytes)
            .map_err(|e| format!("invalid managed runtime response: {e}"))?;
        if response.request_id != request_id || response.kind != kind {
            return Err(format!(
                "mismatched response: expected {kind}/{request_id}, got {}/{}",
                response.kind, response.request_id
            ));
        }
        Ok(response)
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
        let node = Self::decode_response_tree(response.serialized, true)?.ok_or_else(|| {
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
        wire: Option<super::super::WireDecorationNode>,
        required: bool,
    ) -> Result<Option<super::super::DecorationNode>, DecorationEvaluationError> {
        let Some(wire) = wire else {
            return if required {
                Err(DecorationEvaluationError::RuntimeProtocol(
                    "missing serialized composition tree".into(),
                ))
            } else {
                Ok(None)
            };
        };
        // Same conversion/structural validation as the TS wire path.
        let node: super::super::DecorationNode = wire.try_into()?;
        DecorationTree::new(node.clone())
            .validate()
            .map_err(|error| {
                DecorationEvaluationError::RuntimeProtocol(format!(
                    "invalid composition tree: {error:?}"
                ))
            })?;
        Ok(Some(node))
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
        let node = Self::decode_response_tree(response.serialized, false)?;
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

    #[test]
    fn shared_snapshot_fixture_matches_actual_rust_wire() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../../../dotnet/fixtures/window.json"))
                .unwrap();
        assert_eq!(serde_json::to_value(snapshot()).unwrap(), fixture);
    }

    #[test]
    fn malformed_and_mismatched_json_responses_are_errors() {
        for json in [
            "bad json",
            "{}",
            r#"{"requestId":2,"kind":"evaluate","ok":true}"#,
            r#"{"requestId":1,"kind":"unknown","ok":true}"#,
        ] {
            assert!(
                DotNetDecorationEvaluator::decode_response(json.as_bytes(), "evaluate", 1).is_err()
            );
        }
        assert!(
            DotNetDecorationEvaluator::decode_response(
                br#"{"requestId":1,"kind":"evaluate","ok":true}"#,
                "evaluate",
                1
            )
            .is_ok()
        );
    }

    #[test]
    fn invalid_tree_is_rejected() {
        for json in [
            r#"{"kind":"Box"}"#,
            r#"{"kind":"WindowBorder","children":[]}"#,
        ] {
            let wire = serde_json::from_str(json).unwrap();
            assert!(DotNetDecorationEvaluator::decode_response_tree(Some(wire), true).is_err());
        }
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
    fn real_dotnet_host_decodes_example_and_dispatches_delegate() {
        let evaluator = DotNetDecorationEvaluator::new(
            std::env::var_os("SHOJI_TEST_DOTNET_RUNTIME")
                .expect("runtime bootstrap DLL path")
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
