//! Semantic MVP protocol for an external runtime. Transport framing lives in
//! `transport`; snapshots, props and actions reuse the existing serde DTOs.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::super::evaluator::RuntimeWindowAction;
use super::super::{WaylandOutputSnapshot, WaylandWindowSnapshot, WireDecorationNode};
use crate::runtime_input::RuntimeInputDeviceSnapshot;

/// Same camelCase vocabulary as the embedded runtime. Only the external
/// backend serializes this envelope; V8's native requests remain native.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRuntimeRequest<'a> {
    pub request_id: u64,
    pub kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<&'a WaylandWindowSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handler_id: Option<&'a str>,
    pub now_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<&'a str>,
    pub display_state: &'a BTreeMap<String, WaylandOutputSnapshot>,
    pub input_state: &'a BTreeMap<String, RuntimeInputDeviceSnapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRuntimeResponse {
    pub request_id: u64,
    pub kind: String,
    pub ok: bool,
    /// Existing serialized composition format, decoded by WireDecorationNode.
    pub serialized: Option<WireDecorationNode>,
    pub invoked: Option<bool>,
    #[serde(default)]
    pub actions: Vec<RuntimeWindowAction>,
    pub error: Option<String>,
}
