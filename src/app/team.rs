//! Team projection state for the TUI roster and inbox.
//!
//! `TeamSnapshot` / `TeamDelta` are the only authoritative roster/inbox source.
//! The TUI deliberately does not derive agent identity from `spawn_subagent`
//! tool cards or transcript text.

use std::collections::BTreeMap;

use super::*;
use qaqh_client::{
    ClientV2TeamAgentResidency as Residency, ClientV2TeamAgentSnapshot as Agent,
    ClientV2TeamAgentStatus as Status, ClientV2TeamDelivery as Delivery,
    ClientV2TeamDelta as Delta, ClientV2TeamInboxSummary as InboxMessage,
    ClientV2TeamSnapshot as Snapshot,
};

/// Per-root-session view of the backend Team projection.
///
/// `hydrated == false` means no snapshot has been accepted yet. Deltas received
/// in that window are intentionally ignored: they are ephemeral and not replayed,
/// so applying them to an empty map would create a permanently partial roster.
#[derive(Debug, Clone, Default)]
pub struct TeamState {
    hydrated: bool,
    revision: u64,
    last_fact_seq: u64,
    root_session_id: Option<String>,
    /// Primary key is `agent_path`; nickname is display-only.
    agents: BTreeMap<String, Agent>,
    /// Ephemeral deltas can arrive before the durable `AgentJoined` fact.
    /// Keep them keyed by agent id and fold them into the roster entry when it
    /// materializes; dropping them would leave a live child stuck at unloaded.
    pending_status: BTreeMap<String, Status>,
    pending_residency: BTreeMap<String, Residency>,
    inbox: Vec<InboxMessage>,
}

impl TeamState {
    pub fn hydrated(&self) -> bool {
        self.hydrated
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn last_fact_seq(&self) -> u64 {
        self.last_fact_seq
    }

    pub fn root_session_id(&self) -> Option<&str> {
        self.root_session_id.as_deref()
    }

    /// Atomically replace the local projection with an authoritative snapshot.
    pub fn replace_from_snapshot(&mut self, snapshot: Snapshot) {
        self.revision = snapshot.revision;
        self.last_fact_seq = snapshot.last_fact_seq;
        self.root_session_id = snapshot
            .root_session_id
            .as_ref()
            .map(|id| id.as_str().to_owned())
            .or_else(|| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.agent_path.is_root())
                    .map(|agent| agent.agent_id.as_str().to_owned())
            });
        self.agents = snapshot
            .agents
            .into_iter()
            .map(|agent| (agent.agent_path.as_str().to_owned(), agent))
            .collect();
        self.pending_status.clear();
        self.pending_residency.clear();
        self.inbox = snapshot.unread_messages;
        self.hydrated = true;
    }

    /// Apply one ephemeral delta after a snapshot has been accepted.
    ///
    /// Returns `true` when the delta was consumed. Deltas before hydration are
    /// dropped by design; the next snapshot carries the full state.
    pub fn apply_delta(&mut self, delta: Delta) -> bool {
        if !self.hydrated {
            return false;
        }
        match delta {
            Delta::AgentJoined { revision, agent } => {
                self.revision = self.revision.max(revision);
                let mut agent = *agent;
                let agent_id = agent.agent_id.as_str().to_owned();
                if let Some(status) = self.pending_status.remove(&agent_id) {
                    agent.status = status;
                }
                if let Some(residency) = self.pending_residency.remove(&agent_id) {
                    agent.residency = residency;
                }
                self.agents
                    .insert(agent.agent_path.as_str().to_owned(), agent);
            }
            Delta::AgentStatusChanged {
                revision,
                agent_id,
                status,
            } => {
                self.revision = self.revision.max(revision);
                let agent_id = agent_id.as_str().to_owned();
                if let Some(agent) = self.agent_mut_by_id(&agent_id) {
                    agent.status = status;
                } else {
                    self.pending_status.insert(agent_id, status);
                }
            }
            Delta::AgentResidencyChanged {
                revision,
                agent_id,
                residency,
            } => {
                self.revision = self.revision.max(revision);
                let agent_id = agent_id.as_str().to_owned();
                if let Some(agent) = self.agent_mut_by_id(&agent_id) {
                    agent.residency = residency;
                } else {
                    self.pending_residency.insert(agent_id, residency);
                }
            }
            Delta::AgentMessageQueued { revision, message } => {
                self.revision = self.revision.max(revision);
                if !self
                    .inbox
                    .iter()
                    .any(|entry| entry.message_id == message.message_id)
                {
                    self.inbox.push(*message);
                }
            }
            Delta::AgentMessageDelivered {
                revision,
                message_id,
            } => {
                self.revision = self.revision.max(revision);
                self.inbox.retain(|entry| entry.message_id != message_id);
            }
            Delta::AgentInterrupted { revision, agent_id } => {
                self.revision = self.revision.max(revision);
                let agent_id = agent_id.as_str().to_owned();
                if let Some(agent) = self.agent_mut_by_id(&agent_id) {
                    agent.status = Status::Interrupted;
                } else {
                    self.pending_status.insert(agent_id, Status::Interrupted);
                }
            }
            Delta::AgentCompleted {
                revision,
                agent_id,
                status,
            } => {
                self.revision = self.revision.max(revision);
                let agent_id = agent_id.as_str().to_owned();
                if let Some(agent) = self.agent_mut_by_id(&agent_id) {
                    agent.status = status;
                    agent.residency = Residency::Unloaded;
                } else {
                    self.pending_status.insert(agent_id.clone(), status);
                    self.pending_residency.insert(agent_id, Residency::Unloaded);
                }
            }
            Delta::TaskChanged { revision, .. } | Delta::BoardChanged { revision, .. } => {
                self.revision = self.revision.max(revision);
            }
        }
        true
    }

    fn agent_mut_by_id(&mut self, agent_id: &str) -> Option<&mut Agent> {
        self.agents
            .values_mut()
            .find(|agent| agent.agent_id.as_str() == agent_id)
    }

    pub fn agent_by_id(&self, agent_id: &str) -> Option<&Agent> {
        self.agents
            .values()
            .find(|agent| agent.agent_id.as_str() == agent_id)
    }

    pub fn agent_by_path(&self, agent_path: &str) -> Option<&Agent> {
        self.agents.get(agent_path)
    }

    /// Backfill a live status missed before the child timeline attach.
    ///
    /// Team projection remains the roster identity source. This only consumes
    /// the child bootstrap's canonical control activity so a `running` delta
    /// published before attach is not permanently lost. Terminal Team states
    /// are never overwritten by a late/stale control hint.
    pub fn apply_status_hint(&mut self, agent_id: &str, status: Status) -> bool {
        if status_is_terminal(status) {
            return false;
        }
        let Some(agent) = self.agent_mut_by_id(agent_id) else {
            self.pending_status.insert(agent_id.to_owned(), status);
            return true;
        };
        if status_is_terminal(agent.status) || agent.status == status {
            return false;
        }
        agent.status = status;
        true
    }

    /// Roster in stable `agent_path` order, optionally filtered by a path prefix.
    pub fn roster(&self, path_prefix: Option<&str>) -> Vec<&Agent> {
        self.agents
            .values()
            .filter(|agent| {
                path_prefix.is_none_or(|prefix| agent.agent_path.as_str().starts_with(prefix))
            })
            .collect()
    }

    pub fn inbox(&self) -> &[InboxMessage] {
        &self.inbox
    }

    pub fn parent_agent_id(&self, agent_id: &str) -> Option<String> {
        let parent_path = self.agent_by_id(agent_id)?.parent_agent_path.as_ref()?;
        self.agent_by_path(parent_path.as_str())
            .map(|parent| parent.agent_id.as_str().to_owned())
    }

    /// Non-root agents that should have a live timeline subscription.
    pub fn trackable_agent_ids(&self) -> Vec<String> {
        let Some(root) = self.root_session_id.as_deref() else {
            return Vec::new();
        };
        self.agents
            .values()
            .filter(|agent| {
                agent.agent_id.as_str() != root
                    && agent.residency == Residency::Loaded
                    && !status_is_terminal(agent.status)
            })
            .map(|agent| agent.agent_id.as_str().to_owned())
            .collect()
    }

    /// Candidates for `@` completion. Root is omitted because mentions address
    /// collaborators, not the session itself.
    pub fn mention_candidates(&self) -> Vec<&Agent> {
        let root = self.root_session_id.as_deref();
        self.agents
            .values()
            .filter(|agent| Some(agent.agent_id.as_str()) != root)
            .collect()
    }
}

pub fn status_is_terminal(status: Status) -> bool {
    matches!(
        status,
        Status::Completed | Status::Errored | Status::Shutdown | Status::NotFound
    )
}

pub fn status_label(status: Status) -> &'static str {
    match status {
        Status::PendingInit => "pending",
        Status::Running => "running",
        Status::WaitingUser => "waiting_user",
        Status::Interrupted => "interrupted",
        Status::Completed => "completed",
        Status::Errored => "errored",
        Status::Shutdown => "shutdown",
        Status::NotFound => "not_found",
    }
}

pub fn residency_label(residency: Residency) -> &'static str {
    match residency {
        Residency::Loaded => "loaded",
        Residency::Unloaded => "unloaded",
    }
}

pub fn delivery_label(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Queue => "queue",
        Delivery::Trigger => "trigger",
        Delivery::Interrupt => "interrupt",
        Delivery::Steer => "steer",
        Delivery::Interject => "interject",
    }
}

impl App {
    /// Fetch the authoritative roster/inbox snapshot for a root session.
    pub fn fetch_team(&mut self, seed: String) {
        if self.runtime.client_opt().is_none() {
            return;
        }
        self.spawn_api(move |api, tx| async move {
            let result = api.team_v2(&seed).await;
            let _ = tx.send(AppMsg::Action(ActionResult::Team { seed, result }));
        });
    }

    /// Apply an ephemeral TeamDelta only after the root snapshot was accepted.
    ///
    /// Deltas for a child are delivered on that child's per-seed stream; find
    /// the owning root projection before folding them into the roster.
    pub(super) fn handle_team_delta(&mut self, seed: String, delta: Delta) {
        let root = if self.teams.contains_key(&seed) {
            seed.clone()
        } else {
            self.teams
                .iter()
                .find(|(_, state)| state.agent_by_id(&seed).is_some())
                .map(|(root, _)| root.clone())
                .unwrap_or(seed)
        };
        let applied = self
            .teams
            .get_mut(&root)
            .is_some_and(|state| state.apply_delta(delta));
        if applied {
            self.reconcile_team_tracking(&root);
            self.force_redraw = true;
        }
    }

    /// Fold a child bootstrap's canonical activity into the owning Team view.
    ///
    /// `TeamDelta` is ephemeral and may have been published before the child
    /// timeline was attached. Identity still comes exclusively from
    /// `TeamSnapshot/TeamDelta`; this only repairs the live status display.
    pub(super) fn apply_child_activity_hint(
        &mut self,
        child: &str,
        activity: qaqh_client::ClientV2ActivityState,
    ) {
        let status = match activity {
            qaqh_client::ClientV2ActivityState::Running => Status::Running,
            qaqh_client::ClientV2ActivityState::Interrupted => Status::Interrupted,
            qaqh_client::ClientV2ActivityState::Idle => return,
        };
        let Some(root) = self
            .teams
            .iter()
            .find(|(_, state)| state.agent_by_id(child).is_some())
            .map(|(root, _)| root.clone())
        else {
            self.pending_team_status.insert(child.to_owned(), status);
            return;
        };
        let applied = self
            .teams
            .get_mut(&root)
            .is_some_and(|state| state.apply_status_hint(child, status));
        if applied {
            self.reconcile_team_tracking(&root);
            self.force_redraw = true;
            self.flush_pending_team_status();
        }
    }

    /// Apply live status hints buffered before the owning Team snapshot arrived.
    pub(super) fn flush_pending_team_status(&mut self) {
        let pending: Vec<(String, Status)> = self
            .pending_team_status
            .iter()
            .map(|(agent_id, status)| (agent_id.clone(), *status))
            .collect();
        for (agent_id, status) in pending {
            let Some(root) = self
                .teams
                .iter()
                .find(|(_, state)| state.agent_by_id(&agent_id).is_some())
                .map(|(root, _)| root.clone())
            else {
                continue;
            };
            self.pending_team_status.remove(&agent_id);
            let applied = self
                .teams
                .get_mut(&root)
                .is_some_and(|state| state.apply_status_hint(&agent_id, status));
            if applied {
                self.reconcile_team_tracking(&root);
                self.force_redraw = true;
            }
        }
    }

    /// Keep child timeline subscriptions aligned with the Team projection.
    ///
    /// The roster itself is never pruned here: unloaded and terminal agents stay
    /// visible. Only the live timeline attachment is reconciled.
    pub(super) fn reconcile_team_tracking(&mut self, root: &str) {
        let trackable: Vec<String> = self
            .teams
            .get(root)
            .filter(|state| state.hydrated())
            .map(TeamState::trackable_agent_ids)
            .unwrap_or_default();
        for agent_id in &trackable {
            self.ensure_subagent_tracked(root, agent_id);
        }

        let stale: Vec<String> = self
            .subagent_seeds
            .iter()
            .filter(|agent_id| !trackable.iter().any(|candidate| candidate == *agent_id))
            .cloned()
            .collect();
        for agent_id in stale {
            self.untrack_subagent(&agent_id);
        }
    }

    pub fn team_state(&self, seed: &str) -> Option<&TeamState> {
        self.teams.get(seed)
    }

    pub fn active_team_state(&self) -> Option<&TeamState> {
        self.active_seed().and_then(|seed| self.team_state(&seed))
    }

    /// Find the team projection that owns an agent id.
    pub fn team_for_agent(&self, agent_id: &str) -> Option<(&str, &TeamState)> {
        self.teams
            .iter()
            .find_map(|(seed, state)| state.agent_by_id(agent_id).map(|_| (seed.as_str(), state)))
    }

    pub fn child_agent_ids(&self, parent_id: &str) -> Vec<String> {
        let Some((_root, state)) = self.team_for_agent(parent_id) else {
            return Vec::new();
        };
        let Some(parent_path) = state
            .agent_by_id(parent_id)
            .map(|agent| agent.agent_path.as_str().to_owned())
        else {
            return Vec::new();
        };
        state
            .roster(None)
            .into_iter()
            .filter(|agent| {
                agent
                    .parent_agent_path
                    .as_ref()
                    .is_some_and(|path| path.as_str() == parent_path)
            })
            .map(|agent| agent.agent_id.as_str().to_owned())
            .collect()
    }

    /// Open a child transcript from the `/subagents` roster.
    pub fn workspace_open_subagent(&mut self, filtered_index: usize) {
        let Some(root) = self.active_seed() else {
            return;
        };
        let filter = match self.overlays.last() {
            Some(Overlay::Subagents { filter, .. }) => filter.clone(),
            _ => String::new(),
        };
        let Some(agent_id) = self
            .teams
            .get(&root)
            .and_then(|state| state.roster(Some(&filter)).into_iter().nth(filtered_index))
            .map(|agent| agent.agent_id.as_str().to_owned())
        else {
            return;
        };
        if self
            .teams
            .get(&root)
            .and_then(|state| state.root_session_id())
            == Some(agent_id.as_str())
        {
            return;
        }
        let trackable = self
            .teams
            .get(&root)
            .is_some_and(|state| state.trackable_agent_ids().contains(&agent_id));
        if trackable && !self.sessions.contains_key(&agent_id) {
            self.ensure_subagent_tracked(&root, &agent_id);
        }
        if let Some(Overlay::Subagents { .. }) = self.overlays.last() {
            self.overlays.pop();
        }
        self.inspect = Some(agent_id);
        self.force_redraw = true;
    }

    /// Complete the `@` token immediately before the composer cursor.
    ///
    /// Agent paths are inserted because the path is the stable identity; nickname
    /// is accepted as a query alias but is never used as the roster key.
    pub(super) fn autocomplete_mention(&mut self) -> bool {
        let Some(seed) = self.active_seed() else {
            return false;
        };
        let Some(team) = self.teams.get(&seed) else {
            return false;
        };
        let Some(session) = self.sessions.get(&seed) else {
            return false;
        };
        let cursor = session.composer.cursor.min(session.composer.input.len());
        let before: String = session.composer.input.iter().take(cursor).collect();
        let Some(at) = before.rfind('@') else {
            return false;
        };
        let query = &before[at + 1..];
        if query.contains(char::is_whitespace) {
            return false;
        }
        let query_lower = query.to_ascii_lowercase();
        let Some(agent) = team.mention_candidates().into_iter().find(|agent| {
            agent
                .agent_path
                .as_str()
                .to_ascii_lowercase()
                .contains(&query_lower)
                || agent
                    .nickname
                    .as_deref()
                    .is_some_and(|nickname| nickname.to_ascii_lowercase().contains(&query_lower))
        }) else {
            return false;
        };
        let replacement: Vec<char> = format!("@{} ", agent.agent_path.as_str()).chars().collect();
        let Some(session) = self.active_session_mut() else {
            return false;
        };
        session
            .composer
            .input
            .splice(at..cursor, replacement.iter().copied());
        session.composer.cursor = at + replacement.len();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> Snapshot {
        serde_json::from_value(json!({
            "root_session_id": "root-seed",
            "agents": [
                {
                    "agent_id": "root-seed",
                    "agent_path": "/root",
                    "role": "root",
                    "status": "running",
                    "residency": "loaded"
                },
                {
                    "agent_id": "child-seed",
                    "agent_path": "/root/reviewer",
                    "nickname": "reviewer",
                    "role": "review",
                    "status": "running",
                    "residency": "loaded",
                    "parent_agent_path": "/root"
                }
            ],
            "unread_messages": [
                {
                    "message_id": "msg-1",
                    "author": "/root",
                    "recipient": "/root/reviewer",
                    "task_id": "task-1",
                    "delivery": "steer",
                    "created_at_ms": 1
                }
            ],
            "revision": 7,
            "last_fact_seq": 42
        }))
        .expect("team snapshot")
    }

    #[test]
    fn snapshot_is_keyed_by_agent_path_and_preserves_unloaded_state() {
        let mut state = TeamState::default();
        assert!(!state.hydrated());
        assert!(
            !state.apply_delta(
                serde_json::from_value(json!({
                    "kind": "agent_residency_changed",
                    "data": {
                        "revision": 8,
                        "agent_id": "child-seed",
                        "residency": "unloaded"
                    }
                }))
                .expect("delta")
            )
        );

        state.replace_from_snapshot(snapshot());
        assert!(state.hydrated());
        assert!(
            state.apply_delta(
                serde_json::from_value(json!({
                    "kind": "agent_residency_changed",
                    "data": {
                        "revision": 8,
                        "agent_id": "child-seed",
                        "residency": "unloaded"
                    }
                }))
                .expect("delta")
            )
        );

        let child = state
            .agent_by_path("/root/reviewer")
            .expect("unloaded child remains in roster");
        assert_eq!(child.agent_id.as_str(), "child-seed");
        assert_eq!(child.status, Status::Running);
        assert_eq!(child.residency, Residency::Unloaded);
        assert_eq!(state.roster(None).len(), 2);
    }

    #[test]
    fn completed_and_residency_are_orthogonal_updates() {
        let mut state = TeamState::default();
        state.replace_from_snapshot(snapshot());

        assert!(
            state.apply_delta(
                serde_json::from_value(json!({
                    "kind": "agent_completed",
                    "data": {
                        "revision": 9,
                        "agent_id": "child-seed",
                        "status": "completed"
                    }
                }))
                .expect("delta")
            )
        );

        let child = state.agent_by_id("child-seed").expect("child");
        assert_eq!(child.status, Status::Completed);
        assert_eq!(child.residency, Residency::Unloaded);
        assert!(
            state
                .roster(None)
                .iter()
                .any(|agent| { agent.agent_id.as_str() == "child-seed" })
        );
    }

    #[test]
    fn bootstrap_activity_hint_recovers_running_without_resurrecting_terminal() {
        let mut state = TeamState::default();
        let mut initial = snapshot();
        initial.agents[1].status = Status::PendingInit;
        state.replace_from_snapshot(initial);

        assert!(state.apply_status_hint("child-seed", Status::Running));
        assert_eq!(
            state.agent_by_id("child-seed").unwrap().status,
            Status::Running
        );
        assert!(!state.apply_status_hint("child-seed", Status::Running));

        assert!(
            state.apply_delta(
                serde_json::from_value(json!({
                    "kind": "agent_completed",
                    "data": {
                        "revision": 9,
                        "agent_id": "child-seed",
                        "status": "completed"
                    }
                }))
                .expect("delta")
            )
        );
        assert!(!state.apply_status_hint("child-seed", Status::Running));
        assert_eq!(
            state.agent_by_id("child-seed").unwrap().status,
            Status::Completed
        );
    }

    #[test]
    fn delivered_message_leaves_inbox() {
        let mut state = TeamState::default();
        state.replace_from_snapshot(snapshot());
        assert_eq!(state.inbox().len(), 1);

        assert!(
            state.apply_delta(
                serde_json::from_value(json!({
                    "kind": "agent_message_delivered",
                    "data": {
                        "revision": 8,
                        "message_id": "msg-1"
                    }
                }))
                .expect("delta")
            )
        );
        assert!(state.inbox().is_empty());
    }

    #[test]
    fn residency_before_agent_joined_is_folded_into_the_entry() {
        let mut state = TeamState::default();
        state.replace_from_snapshot(
            serde_json::from_value(json!({
                "root_session_id": "root-seed",
                "agents": [
                    {
                        "agent_id": "root-seed",
                        "agent_path": "/root",
                        "status": "running",
                        "residency": "loaded"
                    }
                ],
                "unread_messages": [],
                "revision": 1,
                "last_fact_seq": 1
            }))
            .expect("root snapshot"),
        );

        assert!(
            state.apply_delta(
                serde_json::from_value(json!({
                    "kind": "agent_residency_changed",
                    "data": {
                        "revision": 2,
                        "agent_id": "child-seed",
                        "residency": "loaded"
                    }
                }))
                .expect("residency delta")
            )
        );
        assert!(
            state.apply_delta(
                serde_json::from_value(json!({
                    "kind": "agent_joined",
                    "data": {
                        "revision": 3,
                        "agent": {
                            "agent_id": "child-seed",
                            "agent_path": "/root/reviewer",
                            "status": "pending_init",
                            "residency": "unloaded",
                            "parent_agent_path": "/root"
                        }
                    }
                }))
                .expect("joined delta")
            )
        );

        let child = state.agent_by_id("child-seed").expect("child");
        assert_eq!(child.residency, Residency::Loaded);
        assert_eq!(child.status, Status::PendingInit);
    }

    #[test]
    fn roster_prefix_filter_and_mentions_exclude_root() {
        let mut state = TeamState::default();
        state.replace_from_snapshot(snapshot());

        let filtered = state.roster(Some("/root/review"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].agent_path.as_str(), "/root/reviewer");
        assert_eq!(state.mention_candidates().len(), 1);
        assert_eq!(
            state.mention_candidates()[0].agent_id.as_str(),
            "child-seed"
        );
    }

    #[test]
    fn mention_completion_uses_agent_path_not_nickname_as_key() {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("root-seed".into());
        app.sessions
            .insert("root-seed".into(), SessionState::new("root-seed".into()));
        app.teams
            .entry("root-seed".into())
            .or_default()
            .replace_from_snapshot(snapshot());
        let session = app.sessions.get_mut("root-seed").expect("session");
        session.composer.input = "@rev".chars().collect();
        session.composer.cursor = session.composer.input.len();

        assert!(app.autocomplete_mention());
        assert_eq!(
            app.sessions["root-seed"].composer.value(),
            "@/root/reviewer "
        );
    }

    #[tokio::test]
    async fn child_seed_delta_is_folded_into_the_owning_root_projection() {
        let (mut app, _rx) = App::new_for_test();
        app.teams
            .entry("root-seed".into())
            .or_default()
            .replace_from_snapshot(snapshot());
        app.handle_team_delta(
            "child-seed".into(),
            serde_json::from_value(json!({
                "kind": "agent_status_changed",
                "data": {
                    "revision": 8,
                    "agent_id": "child-seed",
                    "status": "running"
                }
            }))
            .expect("delta"),
        );

        assert_eq!(
            app.teams["root-seed"]
                .agent_by_id("child-seed")
                .expect("child")
                .status,
            Status::Running
        );
    }
}
