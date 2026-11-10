//! The local MCP bridge as the technician's rail shows it (ST-M009-008).
//!
//! The rail of the live session (`#t-live`) names the AI clients that called
//! over local IPC during this session, by client label (ST-M009-011), the
//! number of remote tools the bridge offers, and whether the bridge's IPC
//! endpoint is ready. The endpoint is bound when the session reaches full
//! control and released when it ends; a bind that fails (another CretLink
//! console already owns the endpoint) is reported in plain words with a next
//! step, never hidden: the terminal still works without it.
//!
//! [`get_bridge_status`] answers the frontend, and every change is emitted as
//! a `console-bridge` event ([`BRIDGE_EVENT`]).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cretlink_console::{AuditEntry, client_label};
use cretlink_shared::Actor;
use serde::Serialize;
use tauri::State;

use super::AppState;

/// The event every changed [`BridgeStatus`] is emitted on.
pub(crate) const BRIDGE_EVENT: &str = "console-bridge";

/// The audit action of a bridge connection, which is not a call.
const CONNECT_ACTION: &str = "mcp_bridge_connect";

/// Whether AI clients can reach this console now.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeEndpoint {
    /// The IPC endpoint is bound to the consented session.
    Ready,
    /// It is not: no session is under full control, or the bind failed.
    Unavailable,
}

/// What [`get_bridge_status`] returns (`BridgeStatus` in the frontend
/// contract).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    /// Whether the endpoint is ready.
    pub endpoint: BridgeEndpoint,
    /// Why it is unavailable, with the next step, in plain words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The number of remote tools the bridge offers.
    pub tools: usize,
    /// The labels of the AI clients that called over IPC during this session,
    /// in the order they first called ("Claude Code", "Codex CLI", "Gemini
    /// CLI", "AI assistant").
    pub clients: Vec<String>,
}

/// Where the endpoint is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Endpoint {
    /// No session is under full control, so nothing is bound.
    #[default]
    NotStarted,
    /// Bound to the consented session.
    Ready,
    /// The bind failed, with the reason.
    Failed(String),
}

/// The bridge's state for this session.
#[derive(Debug, Default)]
pub(crate) struct BridgeState {
    endpoint: Endpoint,
    clients: Vec<String>,
}

impl BridgeState {
    /// A new session: no endpoint yet and no clients.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// The endpoint was bound for the consented session.
    pub(crate) fn ready(&mut self) {
        self.endpoint = Endpoint::Ready;
    }

    /// The endpoint could not be bound.
    pub(crate) fn failed(&mut self, error: impl std::fmt::Display) {
        self.endpoint = Endpoint::Failed(error.to_string());
    }

    /// The session ended and the endpoint was released. The clients stay
    /// until the next session starts, like the feed.
    pub(crate) fn closed(&mut self) {
        self.endpoint = Endpoint::NotStarted;
    }

    /// Notes the AI client of one audit record; returns whether the list
    /// changed. Only calls count, not bare connections.
    pub(crate) fn note(&mut self, entry: &AuditEntry) -> bool {
        if entry.actor != Actor::Mcp || entry.action == CONNECT_ACTION {
            return false;
        }
        let name = entry.args.0.get("client").map(ToString::to_string);
        let label = client_label(name.as_deref());
        if self.clients.iter().any(|known| known == label) {
            return false;
        }
        self.clients.push(label.to_owned());
        true
    }

    /// The status the rail shows, with `tools` remote tools on offer.
    pub(crate) fn status(&self, tools: usize) -> BridgeStatus {
        let (endpoint, message) = match &self.endpoint {
            Endpoint::Ready => (BridgeEndpoint::Ready, None),
            Endpoint::NotStarted => (
                BridgeEndpoint::Unavailable,
                Some("AI assistants can connect once the customer has granted control.".to_owned()),
            ),
            Endpoint::Failed(error) => (
                BridgeEndpoint::Unavailable,
                Some(format!(
                    "AI assistants cannot reach this session: the local connection for them \
                     could not start ({error}). Another CretLink console on this computer may \
                     hold it; close it, then stop and start the session again. The terminal \
                     still works."
                )),
            ),
        };
        BridgeStatus {
            endpoint,
            message,
            tools,
            clients: self.clients.clone(),
        }
    }
}

/// The bridge state shared by the session pump and [`get_bridge_status`].
pub(crate) type BridgeSlot = Arc<Mutex<BridgeState>>;

/// Locks the bridge state, recovering from a poisoned mutex.
pub(crate) fn lock_bridge(slot: &BridgeSlot) -> MutexGuard<'_, BridgeState> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The number of remote tools the bridge offers, from its own tool list.
pub(crate) fn tool_count() -> usize {
    cretlink_mcp::McpBridge::tool_names().len()
}

/// The status of the local MCP bridge for the rail (ST-M009-008).
#[tauri::command]
pub(super) fn get_bridge_status(state: State<'_, AppState>) -> BridgeStatus {
    lock_bridge(&state.bridge).status(tool_count())
}

#[cfg(test)]
mod tests {
    use cretlink_shared::{AuditValue, Outcome};

    use super::*;

    fn call(client: Option<&str>) -> AuditEntry {
        let mut entry = AuditEntry::event(Actor::Mcp, None, "mcp_remote_exec_start", Outcome::Ok);
        if let Some(client) = client {
            entry
                .args
                .insert("client", AuditValue::String(client.to_owned()));
        }
        entry
    }

    #[test]
    fn clients_are_the_labels_that_called_in_first_call_order() {
        let mut bridge = BridgeState::default();
        assert!(bridge.note(&call(Some("codex-mcp-client"))));
        assert!(bridge.note(&call(Some("claude-code"))));
        assert!(!bridge.note(&call(Some("claude-code"))), "already listed");
        assert!(bridge.note(&call(None)));
        assert!(
            !bridge.note(&call(Some("something-else"))),
            "AI assistant again"
        );
        assert_eq!(
            bridge.status(25).clients,
            ["Codex CLI", "Claude Code", "AI assistant"]
        );
    }

    #[test]
    fn connections_and_other_actors_are_not_clients() {
        let mut bridge = BridgeState::default();
        assert!(!bridge.note(&AuditEntry::event(
            Actor::Mcp,
            None,
            CONNECT_ACTION,
            Outcome::Ok
        )));
        assert!(!bridge.note(&AuditEntry::event(
            Actor::Terminal,
            None,
            "terminal_open",
            Outcome::Ok
        )));
        assert!(bridge.status(25).clients.is_empty());
    }

    #[test]
    fn the_endpoint_says_whether_ai_clients_can_connect() {
        let mut bridge = BridgeState::default();
        let idle = bridge.status(3);
        assert_eq!(idle.endpoint, BridgeEndpoint::Unavailable);
        assert!(idle.message.is_some());

        bridge.ready();
        let ready = bridge.status(3);
        assert_eq!(ready.endpoint, BridgeEndpoint::Ready);
        assert_eq!(ready.message, None);
        assert_eq!(ready.tools, 3);

        bridge.failed("address in use");
        let failed = bridge.status(3);
        assert_eq!(failed.endpoint, BridgeEndpoint::Unavailable);
        let message = failed.message.unwrap_or_default();
        assert!(message.contains("address in use"), "{message}");
        assert!(message.contains("close it"), "a next step: {message}");

        // The session ended: the endpoint is gone, the clients stay.
        bridge.note(&call(Some("claude-code")));
        bridge.closed();
        assert_eq!(bridge.status(3).endpoint, BridgeEndpoint::Unavailable);
        assert_eq!(bridge.status(3).clients, ["Claude Code"]);
        bridge.reset();
        assert!(bridge.status(3).clients.is_empty());
    }

    #[test]
    fn the_status_serialises_to_the_contract() {
        let mut bridge = BridgeState::default();
        bridge.ready();
        bridge.note(&call(Some("gemini-cli")));
        let json = serde_json::to_value(bridge.status(25)).unwrap_or_default();
        assert_eq!(
            json,
            serde_json::json!({ "endpoint": "ready", "tools": 25, "clients": ["Gemini CLI"] })
        );
        let json = serde_json::to_value(BridgeState::default().status(25)).unwrap_or_default();
        assert_eq!(json["endpoint"], "unavailable");
        assert!(json["message"].is_string());
    }

    #[test]
    fn the_tool_count_is_the_bridge_list() {
        assert_eq!(tool_count(), cretlink_mcp::McpBridge::tool_names().len());
        assert!(tool_count() > 0);
    }
}
