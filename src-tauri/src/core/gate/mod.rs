//! Gate registry: dangerous tool calls pause for explicit approval.
//!
//! Gated operations register a matcher + handler ([`GateRule`]) in the
//! [`GateRegistry`]; nothing matched by the registry ever executes without the
//! user's verdict. A matching permission request becomes a [`PendingGate`] and a
//! `gate.pending` bus event; the UI answers via [`GateManager::respond`], which
//! substitutes the (possibly edited) params back into the tool args before the
//! call is allowed through. Adding a new gated action means writing a new rule
//! and one `register` call — core dispatch code stays untouched.

mod rules;

pub use rules::{GhPrCreateRule, GitCommitRule, GitPushRule};

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::agent::AgentEngine;
use crate::core::bus::{Event, EventBus};
use crate::core::store::Store;
use crate::error::{MaestroError, Result};

/// Settings key: when `"true"`, `git commit` is gated too (push/PR always are).
pub const SETTING_GATE_COMMIT: &str = "gate_commit";

/// One user-editable value extracted from a gated command (e.g. the commit
/// message, or the PR title/body).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateParam {
    pub key: String,
    pub label: String,
    pub value: String,
    pub multiline: bool,
}

/// Successful match: what kind of operation was caught and which params the
/// user may edit before approval.
#[derive(Clone, Debug, PartialEq)]
pub struct GateMatch {
    /// Human-readable label shown in the dialog ("git push", "PR creation", …).
    pub kind: String,
    pub params: Vec<GateParam>,
    /// Set when the gate is deliberately not editable (nested command, several
    /// commits in one line, message read from a file): the dialog then shows the raw
    /// command with Allow/Deny plus this explanation.
    pub note: Option<String>,
}

/// A gated operation: matcher on (tool, args) plus the substitution handler.
pub trait GateRule: Send + Sync {
    fn id(&self) -> &str;
    fn matches(&self, tool: &str, args: &Value) -> Option<GateMatch>;
    /// Substitute the (possibly edited) params back into the tool args; the
    /// result replaces the original args when the call is allowed.
    fn apply(&self, args: &Value, edited: &[GateParam]) -> Value;
}

/// Ordered rule set; the first matching rule wins.
#[derive(Default)]
pub struct GateRegistry {
    rules: Vec<Box<dyn GateRule>>,
}

impl GateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, rule: Box<dyn GateRule>) {
        tracing::debug!(rule = rule.id(), "gate rule registered");
        self.rules.push(rule);
    }

    /// Best match across all rules. Several rules can match one command line
    /// (`git push … && gh pr create …`); the one with editable params wins so the user
    /// still edits the PR title/body, and ties break on registration order.
    pub fn match_tool(&self, tool: &str, args: &Value) -> Option<(&dyn GateRule, GateMatch)> {
        let mut best: Option<(&dyn GateRule, GateMatch)> = None;
        for rule in &self.rules {
            let Some(candidate) = rule.matches(tool, args) else {
                continue;
            };
            let better = match &best {
                None => true,
                Some((_, current)) => candidate.params.len() > current.params.len(),
            };
            if better {
                best = Some((rule.as_ref(), candidate));
            }
        }
        best
    }

    fn rule(&self, id: &str) -> Option<&dyn GateRule> {
        self.rules
            .iter()
            .find(|rule| rule.id() == id)
            .map(|rule| rule.as_ref())
    }
}

/// Built-in rules, honoring the `gate_commit` setting. Called once at startup.
pub fn build_registry(store: &dyn Store) -> GateRegistry {
    let mut registry = GateRegistry::new();
    registry.register(Box::new(GitPushRule));
    registry.register(Box::new(GhPrCreateRule));
    match store.get_setting(SETTING_GATE_COMMIT) {
        Ok(Some(value)) if value == "true" => registry.register(Box::new(GitCommitRule)),
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(error = %err, "failed to read {SETTING_GATE_COMMIT}; commit gate off")
        }
    }
    registry
}

/// A tool call paused at the gate, waiting for the user's verdict.
#[derive(Clone, Debug, Serialize)]
pub struct PendingGate {
    pub gate_id: String,
    pub request_id: String,
    pub session_id: String,
    pub branch: String,
    pub rule_id: String,
    pub kind: String,
    pub tool: String,
    pub params: Vec<GateParam>,
    /// Why this gate is approve-as-is (no editable params); `None` when editable.
    pub note: Option<String>,
    #[serde(rename = "raw_args")]
    pub original_args: Value,
    pub created_at: DateTime<Utc>,
}

/// Owns the registry and the pending-gate map; resolves gates against the
/// agent engine's permission channel.
pub struct GateManager {
    registry: GateRegistry,
    engine: Arc<dyn AgentEngine>,
    bus: EventBus,
    pending: Mutex<HashMap<String, PendingGate>>,
}

impl GateManager {
    pub fn new(registry: GateRegistry, engine: Arc<dyn AgentEngine>, bus: EventBus) -> Self {
        Self {
            registry,
            engine,
            bus,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Try to gate a permission request. On a match the gate is recorded, a
    /// `gate.pending` event is published, and `true` is returned — the caller
    /// must then *not* forward the plain permission request. `false` means no
    /// rule matched (or the pending map is unavailable) and the normal
    /// permission flow should proceed.
    pub fn intercept(
        &self,
        session_id: &str,
        branch: &str,
        request_id: &str,
        tool: &str,
        args: &Value,
    ) -> bool {
        let Some((rule, gate_match)) = self.registry.match_tool(tool, args) else {
            return false;
        };
        let gate = PendingGate {
            gate_id: uuid::Uuid::new_v4().to_string(),
            request_id: request_id.to_string(),
            session_id: session_id.to_string(),
            branch: branch.to_string(),
            rule_id: rule.id().to_string(),
            kind: gate_match.kind,
            tool: tool.to_string(),
            params: gate_match.params,
            note: gate_match.note,
            original_args: args.clone(),
            created_at: Utc::now(),
        };
        match self.lock_pending() {
            Ok(mut pending) => {
                pending.insert(gate.gate_id.clone(), gate.clone());
            }
            Err(err) => {
                // Fall back to the plain permission prompt rather than losing
                // the request entirely — it still needs explicit approval there.
                crate::error::report(&self.bus, &err);
                return false;
            }
        }
        tracing::info!(
            gate_id = gate.gate_id,
            rule = gate.rule_id,
            session_id,
            branch,
            "tool call gated, awaiting approval"
        );
        self.bus.publish(Event::GatePending {
            gate_id: gate.gate_id,
            session_id: gate.session_id,
            tool: gate.tool,
            kind: gate.kind,
            branch: gate.branch,
            params: gate.params,
            note: gate.note,
            raw_args: gate.original_args,
        });
        true
    }

    /// Pending gates, oldest first (for UI reload).
    pub fn list(&self) -> Result<Vec<PendingGate>> {
        let pending = self.lock_pending()?;
        let mut gates: Vec<PendingGate> = pending.values().cloned().collect();
        gates.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(gates)
    }

    /// Resolve a pending gate. Allow substitutes the edited params into the
    /// original tool args; deny sends the optional feedback back to the agent.
    /// The entry is removed only after the engine accepted the response.
    pub fn respond(
        &self,
        gate_id: &str,
        allow: bool,
        edited: &[GateParam],
        feedback: Option<String>,
    ) -> Result<()> {
        let gate = self.lock_pending()?.get(gate_id).cloned().ok_or_else(|| {
            MaestroError::InvalidData {
                message: format!("unknown gate: {gate_id}"),
            }
        })?;

        if allow {
            let rule =
                self.registry
                    .rule(&gate.rule_id)
                    .ok_or_else(|| MaestroError::InvalidData {
                        message: format!("gate rule not registered: {}", gate.rule_id),
                    })?;
            let updated_args = rule.apply(&gate.original_args, edited);
            self.engine
                .respond_permission(&gate.request_id, true, Some(updated_args), None)?;
        } else {
            self.engine
                .respond_permission(&gate.request_id, false, None, feedback)?;
        }

        self.lock_pending()?.remove(gate_id);
        tracing::info!(gate_id, allow, rule = gate.rule_id, "gate resolved");
        self.bus.publish(Event::GateResolved {
            gate_id: gate_id.to_string(),
            reason: if allow { "allowed" } else { "denied" }.into(),
        });
        Ok(())
    }

    /// Drop every gate belonging to `session_id`. Called when a session closes,
    /// crashes, or is swept at startup: its permission request is already resolved
    /// (or gone) on the sidecar side, so the dialog must not keep blocking the UI
    /// with a decision that can no longer take effect.
    pub fn cancel_for_session(&self, session_id: &str, reason: &str) {
        let cancelled: Vec<String> = match self.lock_pending() {
            Ok(mut pending) => {
                let ids: Vec<String> = pending
                    .values()
                    .filter(|gate| gate.session_id == session_id)
                    .map(|gate| gate.gate_id.clone())
                    .collect();
                for id in &ids {
                    pending.remove(id);
                }
                ids
            }
            Err(err) => {
                crate::error::report(&self.bus, &err);
                return;
            }
        };
        for gate_id in cancelled {
            tracing::info!(
                gate_id,
                session_id,
                reason,
                "gate cancelled with its session"
            );
            self.bus.publish(Event::GateResolved {
                gate_id,
                reason: reason.to_string(),
            });
        }
    }

    fn lock_pending(&self) -> Result<MutexGuard<'_, HashMap<String, PendingGate>>> {
        self.pending.lock().map_err(|_| MaestroError::InvalidData {
            message: "gate manager lock poisoned".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent::SpawnSessionRequest;
    use crate::core::store::SqliteStore;
    use serde_json::json;

    /// `(request_id, allow, updated_args, message)` as passed to the engine.
    type PermissionCall = (String, bool, Option<Value>, Option<String>);

    /// Engine double that records permission responses.
    #[derive(Default)]
    struct MockEngine {
        perms: Mutex<Vec<PermissionCall>>,
    }

    impl AgentEngine for MockEngine {
        fn spawn_session(&self, _req: SpawnSessionRequest) -> Result<()> {
            Ok(())
        }
        fn send_prompt(&self, _session_id: &str, _prompt: &str) -> Result<()> {
            Ok(())
        }
        fn interrupt(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }
        fn close_session(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }
        fn respond_permission(
            &self,
            request_id: &str,
            allow: bool,
            updated_args: Option<Value>,
            message: Option<String>,
        ) -> Result<()> {
            self.perms
                .lock()
                .unwrap()
                .push((request_id.to_string(), allow, updated_args, message));
            Ok(())
        }

        fn list_models(&self, _cwd: &str) -> Result<()> {
            Ok(())
        }
    }

    fn manager_with(rules: Vec<Box<dyn GateRule>>) -> (GateManager, Arc<MockEngine>, EventBus) {
        let bus = EventBus::new();
        let engine = Arc::new(MockEngine::default());
        let mut registry = GateRegistry::new();
        for rule in rules {
            registry.register(rule);
        }
        (
            GateManager::new(registry, engine.clone(), bus.clone()),
            engine,
            bus,
        )
    }

    #[tokio::test]
    async fn intercept_records_pending_gate_and_publishes_event() {
        let (gates, _engine, bus) = manager_with(vec![Box::new(GitPushRule)]);
        let mut rx = bus.subscribe();

        let args = json!({ "command": "git push origin main" });
        assert!(gates.intercept("s-1", "impl/T-7-x", "req-1", "Bash", &args));

        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "gate.pending");
        let payload = serde_json::to_value(&event).unwrap();
        assert_eq!(payload["data"]["kind"], "git push");
        assert_eq!(payload["data"]["branch"], "impl/T-7-x");
        assert_eq!(
            payload["data"]["raw_args"]["command"],
            "git push origin main"
        );

        let pending = gates.list().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, "req-1");
        assert_eq!(pending[0].rule_id, "git_push");
    }

    #[test]
    fn intercept_lets_unmatched_calls_fall_through() {
        let (gates, _engine, _bus) = manager_with(vec![Box::new(GitPushRule)]);
        assert!(!gates.intercept("s-1", "b", "req-1", "Bash", &json!({ "command": "ls -la" })));
        assert!(!gates.intercept("s-1", "b", "req-2", "Read", &json!({ "file_path": "x" })));
        assert!(gates.list().unwrap().is_empty());
    }

    #[test]
    fn respond_allow_substitutes_edited_params() {
        let (gates, engine, _bus) = manager_with(vec![Box::new(GhPrCreateRule)]);
        let args = json!({ "command": "gh pr create --title old --body draft" });
        assert!(gates.intercept("s-1", "b", "req-7", "Bash", &args));

        let gate_id = gates.list().unwrap()[0].gate_id.clone();
        let mut edited = gates.list().unwrap()[0].params.clone();
        for p in &mut edited {
            if p.key == "title" {
                p.value = "approved title".into();
            }
        }
        gates.respond(&gate_id, true, &edited, None).unwrap();

        let perms = engine.perms.lock().unwrap();
        let (request_id, allow, updated_args, message) = &perms[0];
        assert_eq!(request_id, "req-7");
        assert!(*allow);
        assert!(message.is_none());
        let command = updated_args.as_ref().unwrap()["command"].as_str().unwrap();
        assert!(command.contains("--title 'approved title'"), "{command}");
        drop(perms);

        assert!(gates.list().unwrap().is_empty(), "entry removed");
    }

    #[test]
    fn respond_deny_returns_feedback_to_the_agent() {
        let (gates, engine, _bus) = manager_with(vec![Box::new(GitPushRule)]);
        let args = json!({ "command": "git push --force" });
        assert!(gates.intercept("s-1", "b", "req-9", "Bash", &args));
        let gate_id = gates.list().unwrap()[0].gate_id.clone();

        gates
            .respond(
                &gate_id,
                false,
                &[],
                Some("rebase first, no force pushes".into()),
            )
            .unwrap();

        let perms = engine.perms.lock().unwrap();
        let (request_id, allow, updated_args, message) = &perms[0];
        assert_eq!(request_id, "req-9");
        assert!(!*allow);
        assert!(updated_args.is_none());
        assert_eq!(message.as_deref(), Some("rebase first, no force pushes"));
    }

    #[test]
    fn respond_to_unknown_gate_is_a_typed_error() {
        let (gates, _engine, _bus) = manager_with(vec![Box::new(GitPushRule)]);
        let err = gates
            .respond("nope", true, &[], None)
            .expect_err("unknown gate");
        assert!(matches!(err, MaestroError::InvalidData { .. }));
        assert!(err.to_string().contains("unknown gate"));
    }

    #[test]
    fn build_registry_respects_gate_commit_setting() {
        let commit_args = json!({ "command": "git commit -m 'x'" });

        // Default: commit not gated, push/PR always gated.
        let store = SqliteStore::open_in_memory().unwrap();
        let registry = build_registry(&store);
        assert!(registry.match_tool("Bash", &commit_args).is_none());
        assert!(registry
            .match_tool("Bash", &json!({ "command": "git push" }))
            .is_some());
        assert!(registry
            .match_tool("Bash", &json!({ "command": "gh pr create -t x -b y" }))
            .is_some());

        // Opt-in: gate_commit = "true".
        store.set_setting(SETTING_GATE_COMMIT, "true").unwrap();
        let registry = build_registry(&store);
        let (rule, gate_match) = registry.match_tool("Bash", &commit_args).unwrap();
        assert_eq!(rule.id(), "git_commit");
        assert_eq!(gate_match.params[0].value, "x");
    }
}
