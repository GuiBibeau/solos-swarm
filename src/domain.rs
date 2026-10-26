use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Agent,
    Evaluator,
    Operator,
    Policy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub id: String,
    pub role: Role,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeedSpec {
    pub id: String,
    pub parent_id: Option<String>,
    pub description: String,
    pub predicate: Value,
    pub version: u64,
    pub required: bool,
    pub required_capabilities: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSpec {
    pub goal_id: String,
    pub intent: String,
    pub version: u64,
    pub rubric_version: String,
    pub evaluation_set_version: String,
    pub evaluator_version: String,
    pub evaluator_id: String,
    pub security: Value,
    pub capabilities: BTreeSet<String>,
    pub budget_microusd: u64,
    pub max_depth: u32,
    pub max_agents: usize,
    pub max_open_tasks: usize,
    pub max_needs: usize,
    pub expiry_ms: u64,
    pub initial_needs: Vec<NeedSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalRecord {
    pub spec: GoalSpec,
    pub versions: Vec<GoalSpec>,
    pub status: String,
    pub stop_reason: Option<String>,
    pub operator_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationContract {
    pub goal_version: u64,
    pub need_versions: BTreeMap<String, u64>,
    pub rubric_version: String,
    pub evaluation_set_version: String,
    pub evaluator_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeedDecision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<crate::telemetry::Correlation>,
    pub satisfied: bool,
    pub finding_ids: Vec<String>,
    pub contract: EvaluationContract,
    pub evaluator_id: String,
    pub signature: String,
    pub recorded_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Need {
    pub goal_id: String,
    pub spec: NeedSpec,
    pub versions: Vec<NeedSpec>,
    pub proposed_by: String,
    pub accepted: bool,
    pub satisfied: bool,
    pub acceptance_signature: Option<String>,
    pub decisions: Vec<NeedDecision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Open,
    Claimed,
    Running,
    Completed,
    Failed,
    Expired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub id: String,
    pub need_id: String,
    pub parent_task_id: Option<String>,
    pub input: Value,
    pub required_capabilities: BTreeSet<String>,
    pub capabilities: BTreeSet<String>,
    pub budget_microusd: u64,
    pub expiry_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub goal_id: String,
    pub goal_version: u64,
    pub spec: TaskSpec,
    pub delegator: String,
    pub delegation_intent_id: String,
    pub placement_intent_id: String,
    pub budget_envelope_ref: String,
    pub depth: u32,
    pub allocated_to_children_microusd: u64,
    pub returned_microusd: u64,
    pub return_intents: BTreeMap<String, u64>,
    pub state: TaskState,
    pub claimed_by: Option<String>,
    pub incarnation: u64,
    pub lease_expiry_ms: u64,
    pub fencing_token: u64,
    pub result_ref: Option<String>,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoardEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<crate::telemetry::Correlation>,
    pub schema_version: u32,
    pub goal_id: String,
    pub entry_id: String,
    pub seq: u64,
    pub revision: u64,
    pub author_id: String,
    pub topic: String,
    pub body: Value,
    pub signature: String,
    pub recorded_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSource {
    pub hash: String,
    pub markings: BTreeSet<String>,
    pub trust: u8,
}

/// The authenticated broker constructs and verifies this metadata before execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub producer_id: String,
    pub recorder_id: String,
    pub observed_inputs: Vec<InputSource>,
    pub source_refs: Vec<String>,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<crate::telemetry::Correlation>,
    pub id: String,
    pub schema_version: u32,
    pub goal_id: String,
    pub goal_version: u64,
    pub task_id: Option<String>,
    pub body: Value,
    pub provenance: Provenance,
    pub markings: BTreeSet<String>,
    pub trust: u8,
    pub flagged: bool,
    pub recorded_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Retraction {
    pub finding_id: String,
    pub actor_id: String,
    pub reason: String,
    pub signature: String,
    pub recorded_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Score {
    pub candidate_hash: String,
    pub contract: EvaluationContract,
    pub value: f64,
    pub dimensions: BTreeMap<String, f64>,
    pub evaluator_id: String,
    pub signature: String,
    pub recorded_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<crate::telemetry::Correlation>,
    pub id: String,
    pub goal_id: String,
    pub goal_version: u64,
    pub producer_id: String,
    pub payload: Value,
    pub candidate_hash: String,
    pub finding_ids: Vec<String>,
    pub markings: BTreeSet<String>,
    pub flagged: bool,
    pub recorded_ms: u64,
    pub scores: Vec<Score>,
    pub planning_round_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanningRound {
    pub id: String,
    pub goal_id: String,
    pub goal_version: u64,
    pub producer_id: String,
    pub context_hash: String,
    pub candidate_limit: usize,
    pub introduced: BTreeSet<String>,
    pub evaluated: BTreeSet<String>,
    pub recorded_ms: u64,
    pub recorded_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceAction {
    pub id: String,
    pub goal_id: String,
    pub goal_version: u64,
    pub finding_ids: Vec<String>,
    pub state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DomainCommand {
    RegisterGoal {
        spec: GoalSpec,
    },
    AmendGoal {
        spec: GoalSpec,
    },
    ProposeNeed {
        goal_id: String,
        spec: NeedSpec,
    },
    AcceptNeed {
        goal_id: String,
        need_id: String,
        goal_version: u64,
        signature: String,
    },
    EvaluateNeed {
        goal_id: String,
        need_id: String,
        satisfied: bool,
        finding_ids: Vec<String>,
        contract: EvaluationContract,
        signature: String,
    },
    DelegateTask {
        goal_id: String,
        spec: TaskSpec,
        intent_id: String,
        placement_intent_id: String,
        budget_envelope_ref: String,
    },
    ClaimTask {
        goal_id: String,
        task_id: String,
        incarnation: u64,
        lease_expiry_ms: u64,
        fencing_token: u64,
    },
    StartTask {
        goal_id: String,
        task_id: String,
        fencing_token: u64,
    },
    RenewTask {
        goal_id: String,
        task_id: String,
        fencing_token: u64,
        lease_expiry_ms: u64,
    },
    CompleteTask {
        goal_id: String,
        task_id: String,
        fencing_token: u64,
        result_ref: String,
    },
    FailTask {
        goal_id: String,
        task_id: String,
        fencing_token: u64,
        reason: String,
    },
    ReturnTaskAllocation {
        goal_id: String,
        task_id: String,
        intent_id: String,
        unused_microusd: u64,
    },
    ExpireTask {
        goal_id: String,
        task_id: String,
    },
    AppendBoard {
        goal_id: String,
        entry_id: String,
        topic: String,
        body: Value,
        signature: String,
    },
    ReadBoard {
        goal_id: String,
        subscriber_id: String,
        after_seq: u64,
        limit: usize,
    },
    CommitBoardCursor {
        goal_id: String,
        subscriber_id: String,
        seq: u64,
    },
    AppendFinding {
        goal_id: String,
        task_id: Option<String>,
        body: Value,
        provenance: Provenance,
    },
    RetractFinding {
        goal_id: String,
        finding_id: String,
        reason: String,
        signature: String,
    },
    SubmitAttempt {
        goal_id: String,
        attempt_id: String,
        payload: Value,
        finding_ids: Vec<String>,
        planning_round_id: String,
    },
    OpenPlanningRound {
        goal_id: String,
        round_id: String,
        producer_id: String,
        context_hash: String,
        candidate_limit: usize,
    },
    ScoreAttempt {
        goal_id: String,
        attempt_id: String,
        score: Score,
    },
    BindActionEvidence {
        action: EvidenceAction,
    },
    StopGoal {
        goal_id: String,
        reason: String,
    },
    ResumeGoal {
        goal_id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainState {
    pub schema_version: u32,
    pub revision: u64,
    pub goals: BTreeMap<String, GoalRecord>,
    pub needs: BTreeMap<String, Need>,
    pub tasks: BTreeMap<String, Task>,
    pub boards: BTreeMap<String, Vec<BoardEntry>>,
    pub board_cursors: BTreeMap<String, u64>,
    pub findings: BTreeMap<String, Finding>,
    pub children: BTreeMap<String, BTreeSet<String>>,
    pub retractions: Vec<Retraction>,
    pub retracted_sources: BTreeSet<String>,
    pub attempts: BTreeMap<String, Attempt>,
    pub excluded_candidate_hashes: BTreeMap<String, BTreeSet<String>>,
    pub actions: BTreeMap<String, EvidenceAction>,
    pub planning_rounds: BTreeMap<String, PlanningRound>,
}

impl Default for DomainState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            revision: 0,
            goals: BTreeMap::new(),
            needs: BTreeMap::new(),
            tasks: BTreeMap::new(),
            boards: BTreeMap::new(),
            board_cursors: BTreeMap::new(),
            findings: BTreeMap::new(),
            children: BTreeMap::new(),
            retractions: vec![],
            retracted_sources: BTreeSet::new(),
            attempts: BTreeMap::new(),
            excluded_candidate_hashes: BTreeMap::new(),
            actions: BTreeMap::new(),
            planning_rounds: BTreeMap::new(),
        }
    }
}

pub fn hash_json(value: &impl Serialize) -> Result<String> {
    let canonical = serde_json::to_value(value)?;
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical)?)
    ))
}
fn key(goal_id: &str, id: &str) -> String {
    format!("{goal_id}/{id}")
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_:".contains(&c))
}
fn signature(value: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "verified signature required");
    Ok(())
}

impl DomainState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn goal(&self, id: &str) -> Result<&GoalRecord> {
        self.goals.get(id).context("unknown Goal")
    }
    pub fn task(&self, goal_id: &str, id: &str) -> Result<&Task> {
        self.tasks.get(&key(goal_id, id)).context("unknown Task")
    }
    pub fn need(&self, goal_id: &str, id: &str) -> Result<&Need> {
        self.needs.get(&key(goal_id, id)).context("unknown Need")
    }
    pub fn attempt(&self, goal_id: &str, id: &str) -> Result<&Attempt> {
        self.attempts
            .get(&key(goal_id, id))
            .context("unknown Attempt")
    }
    pub fn latest_planning_round(&self, goal_id: &str, producer: &str) -> Result<&PlanningRound> {
        let version = self.goal(goal_id)?.spec.version;
        self.planning_rounds
            .values()
            .filter(|round| {
                round.goal_id == goal_id
                    && round.goal_version == version
                    && round.producer_id == producer
            })
            .max_by_key(|round| (round.recorded_revision, &round.id))
            .context("no completed broker-recorded planning inference")
    }
    pub fn current_contract(&self, goal_id: &str) -> Result<EvaluationContract> {
        let spec = &self.goal(goal_id)?.spec;
        Ok(EvaluationContract {
            goal_version: spec.version,
            need_versions: self
                .needs
                .values()
                .filter(|n| n.goal_id == goal_id && n.accepted)
                .map(|n| (n.spec.id.clone(), n.spec.version))
                .collect(),
            rubric_version: spec.rubric_version.clone(),
            evaluation_set_version: spec.evaluation_set_version.clone(),
            evaluator_version: spec.evaluator_version.clone(),
        })
    }
    pub fn finding_eligible(&self, id: &str) -> bool {
        let mut pending = vec![id];
        let mut seen = BTreeSet::new();
        while let Some(current) = pending.pop() {
            if !seen.insert(current) {
                continue;
            }
            let Some(finding) = self.findings.get(current) else {
                return false;
            };
            if finding.flagged || self.retracted_sources.contains(current) {
                return false;
            }
            pending.extend(finding.provenance.source_refs.iter().map(String::as_str));
        }
        true
    }
    pub fn attempt_eligible(&self, attempt: &Attempt) -> bool {
        !attempt.flagged
            && self
                .goals
                .get(&attempt.goal_id)
                .is_some_and(|g| g.spec.version == attempt.goal_version)
            && !self
                .excluded_candidate_hashes
                .get(&attempt.goal_id)
                .is_some_and(|set| set.contains(&attempt.candidate_hash))
            && attempt
                .finding_ids
                .iter()
                .all(|id| self.finding_eligible(id))
    }
    pub fn action_eligible(&self, id: &str, now_ms: u64) -> bool {
        self.actions.get(id).is_some_and(|action| {
            action.state == "approved"
                && self.goals.get(&action.goal_id).is_some_and(|goal| {
                    goal.spec.version == action.goal_version
                        && goal.spec.expiry_ms > now_ms
                        && (goal.status == "active" || goal.status == "awaiting_evaluation")
                })
                && action
                    .finding_ids
                    .iter()
                    .all(|id| self.finding_eligible(id))
        })
    }
    fn active(&self, goal_id: &str, now_ms: u64) -> Result<&GoalSpec> {
        let goal = self.goal(goal_id)?;
        ensure!(goal.spec.expiry_ms > now_ms, "Goal expired");
        ensure!(
            goal.status == "active" || goal.status == "awaiting_evaluation",
            "Goal stopped or completed"
        );
        Ok(&goal.spec)
    }
    fn evaluator(&self, actor: &Actor, goal_id: &str) -> Result<()> {
        ensure!(
            actor.role == Role::Evaluator && actor.id == self.goal(goal_id)?.spec.evaluator_id,
            "registered evaluator required"
        );
        Ok(())
    }
    fn sources(&self, goal_id: &str, ids: &[String]) -> Result<(BTreeSet<String>, u8)> {
        let mut markings = BTreeSet::new();
        let mut trust = 2;
        let mut seen = BTreeSet::new();
        for id in ids {
            ensure!(seen.insert(id), "duplicate Finding source");
            let finding = self.findings.get(id).context("unknown Finding source")?;
            ensure!(
                finding.goal_id == goal_id && self.finding_eligible(id),
                "foreign or retracted Finding source"
            );
            markings.extend(finding.markings.iter().cloned());
            trust = trust.min(finding.trust);
        }
        Ok((markings, trust))
    }
    fn validate_goal(spec: &GoalSpec, now_ms: u64) -> Result<()> {
        ensure!(
            valid_id(&spec.goal_id) && !spec.intent.is_empty() && spec.version > 0,
            "invalid Goal"
        );
        ensure!(
            !spec.evaluator_id.is_empty()
                && !spec.rubric_version.is_empty()
                && !spec.evaluation_set_version.is_empty()
                && !spec.evaluator_version.is_empty(),
            "missing evaluation contract"
        );
        ensure!(
            spec.expiry_ms > now_ms
                && spec.max_depth <= 4
                && spec.max_agents > 0
                && spec.max_agents <= 500
                && spec.max_open_tasks > 0
                && spec.max_needs > 0
                && spec.budget_microusd > 0,
            "invalid Goal ceilings"
        );
        ensure!(
            spec.security.is_object() && !spec.capabilities.is_empty(),
            "missing security posture"
        );
        ensure!(
            !spec.initial_needs.is_empty()
                && spec.initial_needs.len() <= spec.max_needs
                && spec.initial_needs.iter().any(|n| n.required),
            "required initial Needs missing"
        );
        let mut ids = BTreeSet::new();
        for need in &spec.initial_needs {
            ensure!(
                valid_id(&need.id)
                    && ids.insert(&need.id)
                    && need.version > 0
                    && !need.description.is_empty()
                    && !need.predicate.is_null(),
                "invalid initial Need"
            );
            ensure!(
                need.required_capabilities.is_subset(&spec.capabilities),
                "Need exceeds Goal authority"
            );
        }
        for need in &spec.initial_needs {
            ensure!(
                need.parent_id
                    .as_ref()
                    .is_none_or(|p| ids.contains(p) && p != &need.id),
                "invalid initial Need parent"
            );
            let mut seen = BTreeSet::new();
            let mut current = Some(&need.id);
            while let Some(id) = current {
                ensure!(seen.insert(id), "Need parent cycle");
                current = spec
                    .initial_needs
                    .iter()
                    .find(|n| &n.id == id)
                    .and_then(|n| n.parent_id.as_ref());
            }
        }
        Ok(())
    }
    fn update_completion(&mut self, goal_id: &str) {
        let required: Vec<_> = self
            .needs
            .values()
            .filter(|n| n.goal_id == goal_id && n.accepted && n.spec.required)
            .collect();
        let complete = !required.is_empty() && required.iter().all(|n| n.satisfied);
        if let Some(goal) = self.goals.get_mut(goal_id)
            && goal.status != "stopped"
        {
            goal.status = if complete { "completed" } else { "active" }.into();
        }
    }
    /// Security verifies credentials, typed grants, signatures and ledger admission first.
    /// The storage state machine commits the returned state/result in one transaction.
    pub fn execute(&mut self, actor: &Actor, command: DomainCommand, now_ms: u64) -> Result<Value> {
        ensure!(self.schema_version == 1, "unsupported domain state schema");
        ensure!(!actor.id.is_empty(), "missing actor");
        if let DomainCommand::ReadBoard {
            goal_id,
            subscriber_id,
            after_seq,
            limit,
        } = command
        {
            ensure!(
                actor.id == subscriber_id || actor.role == Role::Operator,
                "foreign subscription cursor"
            );
            self.goal(&goal_id)?;
            ensure!((1..=128).contains(&limit), "invalid Board batch limit");
            let board = self.boards.get(&goal_id).context("missing Board")?;
            ensure!(after_seq <= board.len() as u64, "cursor beyond Board head");
            let entries: Vec<_> = board
                .iter()
                .filter(|e| e.seq > after_seq)
                .take(limit)
                .collect();
            return Ok(
                json!({"entries":entries,"head_seq":board.len(),"committed_cursor":self.board_cursors.get(&key(&goal_id,&subscriber_id)).copied().unwrap_or(0),"revision":self.revision}),
            );
        }
        let mut next = self.clone();
        next.revision = next.revision.checked_add(1).context("revision exhausted")?;
        let value = next.apply(actor, command, now_ms)?;
        *self = next;
        Ok(value)
    }
    fn apply(&mut self, actor: &Actor, command: DomainCommand, now_ms: u64) -> Result<Value> {
        match command {
            DomainCommand::RegisterGoal { spec } => {
                ensure!(
                    actor.role == Role::Operator,
                    "operator registration required"
                );
                Self::validate_goal(&spec, now_ms)?;
                ensure!(
                    !self.goals.contains_key(&spec.goal_id),
                    "Goal already registered"
                );
                ensure!(
                    !self
                        .goals
                        .values()
                        .any(|g| g.status != "stopped" && g.spec.expiry_ms > now_ms),
                    "one admitted root Goal per deployment"
                );
                for need in &spec.initial_needs {
                    self.needs.insert(
                        key(&spec.goal_id, &need.id),
                        Need {
                            goal_id: spec.goal_id.clone(),
                            spec: need.clone(),
                            versions: vec![need.clone()],
                            proposed_by: actor.id.clone(),
                            accepted: true,
                            satisfied: false,
                            acceptance_signature: None,
                            decisions: vec![],
                        },
                    );
                }
                self.boards.insert(spec.goal_id.clone(), vec![]);
                let id = spec.goal_id.clone();
                self.goals.insert(
                    id.clone(),
                    GoalRecord {
                        versions: vec![spec.clone()],
                        spec,
                        status: "active".into(),
                        stop_reason: None,
                        operator_id: actor.id.clone(),
                    },
                );
                Ok(json!({"goal_id":id,"revision":self.revision}))
            }
            DomainCommand::AmendGoal { spec } => {
                ensure!(actor.role == Role::Operator, "operator amendment required");
                Self::validate_goal(&spec, now_ms)?;
                let old = self.goal(&spec.goal_id)?.spec.clone();
                ensure!(
                    spec.version == old.version + 1,
                    "Goal version must advance exactly once"
                );
                for need in self
                    .needs
                    .values_mut()
                    .filter(|n| n.goal_id == spec.goal_id)
                {
                    need.accepted = false;
                    need.satisfied = false;
                    need.acceptance_signature = None;
                }
                for proposed in &spec.initial_needs {
                    if let Some(need) = self.needs.get_mut(&key(&spec.goal_id, &proposed.id)) {
                        ensure!(
                            proposed.version > need.spec.version,
                            "amended Need version must advance"
                        );
                        need.spec = proposed.clone();
                        need.versions.push(proposed.clone());
                        need.accepted = true;
                        need.satisfied = false;
                    } else {
                        self.needs.insert(
                            key(&spec.goal_id, &proposed.id),
                            Need {
                                goal_id: spec.goal_id.clone(),
                                spec: proposed.clone(),
                                versions: vec![proposed.clone()],
                                proposed_by: actor.id.clone(),
                                accepted: true,
                                satisfied: false,
                                acceptance_signature: None,
                                decisions: vec![],
                            },
                        );
                    }
                }
                for need in self
                    .needs
                    .values_mut()
                    .filter(|n| n.goal_id == spec.goal_id)
                {
                    need.satisfied = false;
                }
                for task in self
                    .tasks
                    .values_mut()
                    .filter(|t| t.goal_id == spec.goal_id && t.state != TaskState::Completed)
                {
                    task.state = TaskState::Expired;
                    task.lease_expiry_ms = 0;
                }
                for action in self
                    .actions
                    .values_mut()
                    .filter(|a| a.goal_id == spec.goal_id)
                {
                    action.state = "reopened".into();
                }
                let goal = self.goals.get_mut(&spec.goal_id).unwrap();
                goal.versions.push(spec.clone());
                goal.spec = spec;
                goal.status = "active".into();
                goal.stop_reason = None;
                Ok(json!({"version":goal.spec.version,"revision":self.revision}))
            }
            DomainCommand::ProposeNeed { goal_id, spec } => {
                let goal = self.active(&goal_id, now_ms)?;
                ensure!(
                    actor.role == Role::Agent || actor.role == Role::Operator,
                    "Need proposer required"
                );
                ensure!(
                    valid_id(&spec.id)
                        && spec.version == 1
                        && !spec.description.is_empty()
                        && !spec.predicate.is_null(),
                    "invalid proposed Need"
                );
                ensure!(
                    spec.required_capabilities.is_subset(&goal.capabilities),
                    "Need exceeds Goal authority"
                );
                ensure!(
                    self.needs.values().filter(|n| n.goal_id == goal_id).count() < goal.max_needs,
                    "Need cap reached"
                );
                ensure!(
                    !self.needs.contains_key(&key(&goal_id, &spec.id)),
                    "Need ID already exists"
                );
                let parent = self.need(
                    &goal_id,
                    spec.parent_id
                        .as_deref()
                        .context("derived Need requires parent")?,
                )?;
                ensure!(parent.accepted, "parent Need is not accepted");
                let id = spec.id.clone();
                self.needs.insert(
                    key(&goal_id, &id),
                    Need {
                        goal_id,
                        spec: spec.clone(),
                        versions: vec![spec],
                        proposed_by: actor.id.clone(),
                        accepted: false,
                        satisfied: false,
                        acceptance_signature: None,
                        decisions: vec![],
                    },
                );
                Ok(json!({"need_id":id,"accepted":false}))
            }
            DomainCommand::AcceptNeed {
                goal_id,
                need_id,
                goal_version,
                signature: sig,
            } => {
                self.active(&goal_id, now_ms)?;
                self.evaluator(actor, &goal_id)?;
                signature(&sig)?;
                ensure!(
                    self.goal(&goal_id)?.spec.version == goal_version,
                    "stale Goal contract"
                );
                ensure!(
                    self.need(&goal_id, &need_id)?
                        .spec
                        .required_capabilities
                        .is_subset(&self.goal(&goal_id)?.spec.capabilities),
                    "Need exceeds amended Goal authority"
                );
                let need = self
                    .needs
                    .get_mut(&key(&goal_id, &need_id))
                    .context("unknown Need")?;
                need.accepted = true;
                need.acceptance_signature = Some(sig);
                self.update_completion(&goal_id);
                Ok(json!({"need_id":need_id,"accepted":true}))
            }
            DomainCommand::EvaluateNeed {
                goal_id,
                need_id,
                satisfied,
                finding_ids,
                contract,
                signature: sig,
            } => {
                self.evaluator(actor, &goal_id)?;
                signature(&sig)?;
                ensure!(
                    self.goal(&goal_id)?.spec.expiry_ms > now_ms
                        && self.goal(&goal_id)?.status != "stopped",
                    "Goal not evaluable"
                );
                ensure!(
                    contract == self.current_contract(&goal_id)?,
                    "stale evaluation contract"
                );
                self.sources(&goal_id, &finding_ids)?;
                ensure!(
                    !satisfied || !finding_ids.is_empty(),
                    "satisfaction requires current evidence"
                );
                let need = self
                    .needs
                    .get_mut(&key(&goal_id, &need_id))
                    .context("unknown Need")?;
                ensure!(need.accepted, "Need not accepted");
                need.satisfied = satisfied;
                need.decisions.push(NeedDecision {
                    correlation: None,
                    satisfied,
                    finding_ids,
                    contract,
                    evaluator_id: actor.id.clone(),
                    signature: sig,
                    recorded_ms: now_ms,
                });
                self.update_completion(&goal_id);
                Ok(
                    json!({"need_id":need_id,"satisfied":satisfied,"goal_status":self.goal(&goal_id)?.status}),
                )
            }
            DomainCommand::DelegateTask {
                goal_id,
                spec,
                intent_id,
                placement_intent_id,
                budget_envelope_ref,
            } => {
                let goal = self.active(&goal_id, now_ms)?.clone();
                ensure!(
                    actor.role == Role::Agent || actor.role == Role::Operator,
                    "Task delegator required"
                );
                ensure!(
                    valid_id(&spec.id)
                        && valid_id(&intent_id)
                        && !placement_intent_id.is_empty()
                        && !budget_envelope_ref.is_empty()
                        && spec.budget_microusd > 0,
                    "invalid Task allocation"
                );
                let need = self.need(&goal_id, &spec.need_id)?;
                ensure!(need.accepted, "Task Need not accepted");
                ensure!(
                    spec.required_capabilities.is_subset(&spec.capabilities),
                    "Task required grants exceed delegated grants"
                );
                if let Some(existing) = self.tasks.get(&key(&goal_id, &spec.id)) {
                    ensure!(
                        existing.delegation_intent_id == intent_id
                            && existing.delegator == actor.id
                            && hash_json(&existing.spec)? == hash_json(&spec)?,
                        "immutable Task conflict"
                    );
                    return Ok(serde_json::to_value(existing)?);
                }
                ensure!(
                    !self
                        .tasks
                        .values()
                        .any(|t| t.goal_id == goal_id && t.delegation_intent_id == intent_id),
                    "delegation intent reused"
                );
                ensure!(
                    self.tasks
                        .values()
                        .filter(|t| t.goal_id == goal_id
                            && matches!(
                                t.state,
                                TaskState::Open | TaskState::Claimed | TaskState::Running
                            ))
                        .count()
                        < goal.max_open_tasks,
                    "Open Task cap reached"
                );
                let depth = if let Some(parent_id) = &spec.parent_task_id {
                    let parent = self.task(&goal_id, parent_id)?;
                    ensure!(
                        parent.claimed_by.as_deref() == Some(&actor.id)
                            && parent.lease_expiry_ms > now_ms
                            && parent.goal_version == goal.version
                            && matches!(parent.state, TaskState::Claimed | TaskState::Running),
                        "delegator does not hold current parent Task"
                    );
                    ensure!(
                        spec.capabilities.is_subset(&parent.spec.capabilities)
                            && spec.expiry_ms <= parent.spec.expiry_ms,
                        "delegation widens authority or expiry"
                    );
                    ensure!(
                        spec.budget_microusd
                            <= parent
                                .spec
                                .budget_microusd
                                .saturating_sub(parent.allocated_to_children_microusd)
                                .saturating_sub(parent.returned_microusd),
                        "delegation exceeds remaining envelope"
                    );
                    parent.depth + 1
                } else {
                    ensure!(
                        actor.role == Role::Operator,
                        "agent root delegation forbidden"
                    );
                    ensure!(
                        spec.capabilities.is_subset(&goal.capabilities),
                        "Task exceeds Goal authority"
                    );
                    let allocated: u64 = self
                        .tasks
                        .values()
                        .filter(|t| {
                            t.goal_id == goal_id
                                && t.goal_version == goal.version
                                && t.spec.parent_task_id.is_none()
                        })
                        .map(|t| t.spec.budget_microusd - t.returned_microusd)
                        .sum();
                    ensure!(
                        spec.budget_microusd <= goal.budget_microusd.saturating_sub(allocated),
                        "root Task budget overcommitted"
                    );
                    0
                };
                ensure!(
                    depth <= goal.max_depth
                        && spec.expiry_ms > now_ms
                        && spec.expiry_ms <= goal.expiry_ms,
                    "Task depth or expiry exceeds Goal"
                );
                if let Some(parent) = &spec.parent_task_id {
                    self.tasks
                        .get_mut(&key(&goal_id, parent))
                        .unwrap()
                        .allocated_to_children_microusd += spec.budget_microusd;
                }
                let task = Task {
                    goal_id: goal_id.clone(),
                    goal_version: goal.version,
                    spec,
                    delegator: actor.id.clone(),
                    delegation_intent_id: intent_id,
                    placement_intent_id,
                    budget_envelope_ref,
                    depth,
                    allocated_to_children_microusd: 0,
                    returned_microusd: 0,
                    return_intents: BTreeMap::new(),
                    state: TaskState::Open,
                    claimed_by: None,
                    incarnation: 0,
                    lease_expiry_ms: 0,
                    fencing_token: 0,
                    result_ref: None,
                    failure_reason: None,
                };
                let value = serde_json::to_value(&task)?;
                self.tasks.insert(key(&goal_id, &task.spec.id), task);
                Ok(value)
            }
            DomainCommand::ClaimTask {
                goal_id,
                task_id,
                incarnation,
                lease_expiry_ms,
                fencing_token,
            } => {
                let goal = self.active(&goal_id, now_ms)?.clone();
                ensure!(actor.role == Role::Agent, "agent claimant required");
                let live: BTreeSet<_> = self
                    .tasks
                    .values()
                    .filter(|t| {
                        t.goal_id == goal_id
                            && t.lease_expiry_ms > now_ms
                            && matches!(t.state, TaskState::Claimed | TaskState::Running)
                    })
                    .filter_map(|t| t.claimed_by.as_ref())
                    .collect();
                ensure!(
                    live.contains(&actor.id) || live.len() < goal.max_agents,
                    "live agent cap reached"
                );
                let task = self
                    .tasks
                    .get_mut(&key(&goal_id, &task_id))
                    .context("unknown Task")?;
                if task.claimed_by.as_deref() == Some(&actor.id)
                    && task.incarnation == incarnation
                    && task.fencing_token == fencing_token
                    && task.lease_expiry_ms > now_ms
                {
                    return Ok(serde_json::to_value(task)?);
                }
                ensure!(
                    task.goal_version == goal.version
                        && task.spec.expiry_ms > now_ms
                        && task.returned_microusd < task.spec.budget_microusd
                        && incarnation > 0,
                    "stale Task allocation"
                );
                ensure!(
                    task.state == TaskState::Open
                        || ((task.state == TaskState::Claimed
                            || task.state == TaskState::Running
                            || task.state == TaskState::Expired)
                            && task.lease_expiry_ms <= now_ms),
                    "Task already owned or terminal"
                );
                ensure!(
                    fencing_token > task.fencing_token
                        && lease_expiry_ms > now_ms
                        && lease_expiry_ms <= task.spec.expiry_ms,
                    "invalid Task lease or fence"
                );
                task.claimed_by = Some(actor.id.clone());
                task.incarnation = incarnation;
                task.fencing_token = fencing_token;
                task.lease_expiry_ms = lease_expiry_ms;
                task.state = TaskState::Claimed;
                Ok(serde_json::to_value(task)?)
            }
            DomainCommand::StartTask {
                goal_id,
                task_id,
                fencing_token,
            } => {
                self.active(&goal_id, now_ms)?;
                let task = self.owned_task_mut(actor, &goal_id, &task_id, fencing_token, now_ms)?;
                ensure!(
                    task.state == TaskState::Claimed || task.state == TaskState::Running,
                    "Task not startable"
                );
                task.state = TaskState::Running;
                Ok(serde_json::to_value(task)?)
            }
            DomainCommand::RenewTask {
                goal_id,
                task_id,
                fencing_token,
                lease_expiry_ms,
            } => {
                self.active(&goal_id, now_ms)?;
                let task = self.owned_task_mut(actor, &goal_id, &task_id, fencing_token, now_ms)?;
                ensure!(
                    lease_expiry_ms >= task.lease_expiry_ms
                        && lease_expiry_ms <= task.spec.expiry_ms,
                    "renewal widens Task expiry"
                );
                task.lease_expiry_ms = lease_expiry_ms;
                Ok(serde_json::to_value(task)?)
            }
            DomainCommand::CompleteTask {
                goal_id,
                task_id,
                fencing_token,
                result_ref,
            } => {
                let goal = self.goal(&goal_id)?;
                ensure!(
                    goal.spec.expiry_ms > now_ms
                        && matches!(
                            goal.status.as_str(),
                            "active" | "awaiting_evaluation" | "completed"
                        ),
                    "Task completion Goal is stopped or expired"
                );
                let finding = self
                    .findings
                    .get(&result_ref)
                    .context("Task result must reference Finding")?;
                ensure!(
                    finding.goal_id == goal_id
                        && finding.goal_version == goal.spec.version
                        && finding.provenance.producer_id == actor.id
                        && finding.task_id.as_deref() == Some(&task_id)
                        && self.finding_eligible(&result_ref),
                    "invalid Task result ownership or lineage"
                );
                let task = self.owned_task_mut(actor, &goal_id, &task_id, fencing_token, now_ms)?;
                if task.state == TaskState::Completed {
                    ensure!(
                        task.result_ref.as_deref() == Some(&result_ref),
                        "immutable Task result conflict"
                    );
                } else {
                    ensure!(
                        task.state == TaskState::Claimed || task.state == TaskState::Running,
                        "Task cannot complete"
                    );
                    task.state = TaskState::Completed;
                    task.result_ref = Some(result_ref);
                }
                Ok(serde_json::to_value(task)?)
            }
            DomainCommand::ExpireTask { goal_id, task_id } => {
                ensure!(
                    actor.role == Role::Operator,
                    "operator lease expiry required"
                );
                let task = self
                    .tasks
                    .get_mut(&key(&goal_id, &task_id))
                    .context("unknown Task")?;
                ensure!(
                    task.lease_expiry_ms <= now_ms && task.state != TaskState::Completed,
                    "Task lease not expired"
                );
                task.state = TaskState::Expired;
                Ok(serde_json::to_value(task)?)
            }
            DomainCommand::FailTask {
                goal_id,
                task_id,
                fencing_token,
                reason,
            } => {
                ensure!(!reason.is_empty(), "Task failure reason required");
                if actor.role != Role::Operator {
                    self.active(&goal_id, now_ms)?;
                    self.owned_task_mut(actor, &goal_id, &task_id, fencing_token, now_ms)?;
                }
                let task = self
                    .tasks
                    .get_mut(&key(&goal_id, &task_id))
                    .context("unknown Task")?;
                ensure!(
                    task.state != TaskState::Completed,
                    "completed Task cannot fail"
                );
                task.state = TaskState::Failed;
                task.failure_reason = Some(reason);
                task.lease_expiry_ms = 0;
                Ok(serde_json::to_value(task)?)
            }
            DomainCommand::ReturnTaskAllocation {
                goal_id,
                task_id,
                intent_id,
                unused_microusd,
            } => {
                ensure!(
                    actor.role == Role::Operator && valid_id(&intent_id),
                    "verified ledger allocation return required"
                );
                let task = self.task(&goal_id, &task_id)?;
                if let Some(previous) = task.return_intents.get(&intent_id) {
                    ensure!(
                        *previous == unused_microusd,
                        "allocation return intent conflict"
                    );
                    return Ok(json!({"returned_microusd":task.returned_microusd}));
                }
                ensure!(
                    !matches!(task.state, TaskState::Claimed | TaskState::Running)
                        || task.lease_expiry_ms <= now_ms,
                    "live allocation cannot be returned"
                );
                ensure!(
                    unused_microusd > 0
                        && unused_microusd
                            <= task
                                .spec
                                .budget_microusd
                                .saturating_sub(task.allocated_to_children_microusd)
                                .saturating_sub(task.returned_microusd),
                    "allocation return exceeds unallocated limit"
                );
                let parent = task.spec.parent_task_id.clone();
                let task = self.tasks.get_mut(&key(&goal_id, &task_id)).unwrap();
                task.returned_microusd += unused_microusd;
                task.return_intents.insert(intent_id, unused_microusd);
                let returned = task.returned_microusd;
                if let Some(parent) = parent {
                    let parent = self
                        .tasks
                        .get_mut(&key(&goal_id, &parent))
                        .context("missing parent envelope")?;
                    parent.allocated_to_children_microusd = parent
                        .allocated_to_children_microusd
                        .checked_sub(unused_microusd)
                        .context("parent allocation inconsistent")?;
                }
                Ok(json!({"returned_microusd":returned,"revision":self.revision}))
            }
            DomainCommand::AppendBoard {
                goal_id,
                entry_id,
                topic,
                body,
                signature: sig,
            } => {
                self.active(&goal_id, now_ms)?;
                signature(&sig)?;
                ensure!(
                    valid_id(&entry_id)
                        && !topic.is_empty()
                        && topic.len() <= 128
                        && serde_json::to_vec(&body)?.len() <= 65_536,
                    "invalid Board entry"
                );
                let board = self.boards.get_mut(&goal_id).context("missing Board")?;
                if let Some(existing) = board.iter().find(|e| e.entry_id == entry_id) {
                    ensure!(
                        existing.author_id == actor.id
                            && existing.topic == topic
                            && existing.body == body,
                        "Board entry ID conflict"
                    );
                    return Ok(serde_json::to_value(existing)?);
                }
                let entry = BoardEntry {
                    correlation: None,
                    schema_version: 1,
                    goal_id,
                    entry_id,
                    seq: board.len() as u64 + 1,
                    revision: self.revision,
                    author_id: actor.id.clone(),
                    topic,
                    body,
                    signature: sig,
                    recorded_ms: now_ms,
                };
                let value = serde_json::to_value(&entry)?;
                board.push(entry);
                Ok(value)
            }
            DomainCommand::CommitBoardCursor {
                goal_id,
                subscriber_id,
                seq,
            } => {
                self.goal(&goal_id)?;
                ensure!(
                    subscriber_id == actor.id || actor.role == Role::Operator,
                    "foreign subscriber cursor"
                );
                ensure!(
                    seq <= self.boards.get(&goal_id).context("missing Board")?.len() as u64,
                    "cursor beyond Board head"
                );
                let cursor = self
                    .board_cursors
                    .entry(key(&goal_id, &subscriber_id))
                    .or_default();
                ensure!(seq >= *cursor, "cursor cannot move backwards");
                *cursor = seq;
                Ok(json!({"seq":seq,"revision":self.revision}))
            }
            DomainCommand::AppendFinding {
                goal_id,
                task_id,
                body,
                provenance,
            } => {
                let goal = self.active(&goal_id, now_ms)?.clone();
                signature(&provenance.signature)?;
                ensure!(
                    provenance.producer_id == actor.id
                        && !provenance.recorder_id.is_empty()
                        && provenance.recorder_id != actor.id,
                    "broker-written producer provenance required"
                );
                if let Some(object) = body.as_object() {
                    ensure!(
                        ![
                            "provenance",
                            "broker_provenance",
                            "producer_id",
                            "markings",
                            "trust",
                            "source_refs",
                            "observed_inputs"
                        ]
                        .iter()
                        .any(|key| object.contains_key(*key)),
                        "agent-supplied provenance rejected"
                    );
                }
                if let Some(task_id) = &task_id {
                    let task = self.task(&goal_id, task_id)?;
                    ensure!(
                        task.claimed_by.as_deref() == Some(&actor.id)
                            && task.lease_expiry_ms > now_ms
                            && task.goal_version == goal.version
                            && matches!(task.state, TaskState::Claimed | TaskState::Running),
                        "Finding producer does not own current Task"
                    );
                } else {
                    ensure!(
                        actor.role == Role::Operator,
                        "agent Finding requires Task ownership"
                    );
                }
                let (mut markings, mut trust) = self.sources(&goal_id, &provenance.source_refs)?;
                ensure!(
                    !provenance.observed_inputs.is_empty() || !provenance.source_refs.is_empty(),
                    "observed input provenance missing"
                );
                for input in &provenance.observed_inputs {
                    ensure!(
                        input.hash.len() == 64
                            && input.hash.bytes().all(|c| c.is_ascii_hexdigit())
                            && input.trust <= 2,
                        "invalid observed input"
                    );
                    markings.extend(input.markings.iter().cloned());
                    trust = trust.min(input.trust);
                }
                let id = hash_json(
                    &json!({"schema_version":1,"goal_id":goal_id,"goal_version":goal.version,"task_id":task_id,"body":body,"provenance":provenance,"markings":markings,"trust":trust}),
                )?;
                if let Some(existing) = self.findings.get(&id) {
                    return Ok(serde_json::to_value(existing)?);
                }
                let finding = Finding {
                    correlation: None,
                    id: id.clone(),
                    schema_version: 1,
                    goal_id,
                    goal_version: goal.version,
                    task_id,
                    body,
                    provenance,
                    markings,
                    trust,
                    flagged: false,
                    recorded_ms: now_ms,
                };
                for parent in &finding.provenance.source_refs {
                    self.children
                        .entry(parent.clone())
                        .or_default()
                        .insert(id.clone());
                }
                let value = serde_json::to_value(&finding)?;
                self.findings.insert(id, finding);
                Ok(value)
            }
            DomainCommand::RetractFinding {
                goal_id,
                finding_id,
                reason,
                signature: sig,
            } => {
                signature(&sig)?;
                ensure!(!reason.is_empty(), "retraction reason required");
                let source = self.findings.get(&finding_id).context("unknown Finding")?;
                ensure!(
                    source.goal_id == goal_id
                        && (actor.role == Role::Operator
                            || actor.id == source.provenance.producer_id
                            || (actor.role == Role::Evaluator
                                && actor.id == self.goal(&goal_id)?.spec.evaluator_id)),
                    "retraction outside actor scope"
                );
                if self.retracted_sources.insert(finding_id.clone()) {
                    self.retractions.push(Retraction {
                        finding_id: finding_id.clone(),
                        actor_id: actor.id.clone(),
                        reason,
                        signature: sig,
                        recorded_ms: now_ms,
                    });
                }
                let mut queue = VecDeque::from([finding_id]);
                let mut affected = BTreeSet::new();
                while let Some(id) = queue.pop_front() {
                    if !affected.insert(id.clone()) {
                        continue;
                    }
                    if let Some(f) = self.findings.get_mut(&id) {
                        f.flagged = true;
                    }
                    if let Some(children) = self.children.get(&id) {
                        queue.extend(children.iter().cloned());
                    }
                }
                let mut reopened = vec![];
                for need in self.needs.values_mut().filter(|n| n.goal_id == goal_id) {
                    if need
                        .decisions
                        .last()
                        .is_some_and(|d| d.finding_ids.iter().any(|id| affected.contains(id)))
                    {
                        need.satisfied = false;
                        reopened.push(need.spec.id.clone());
                    }
                }
                for attempt in self.attempts.values_mut().filter(|a| a.goal_id == goal_id) {
                    if attempt.finding_ids.iter().any(|id| affected.contains(id)) {
                        attempt.flagged = true;
                        self.excluded_candidate_hashes
                            .entry(goal_id.clone())
                            .or_default()
                            .insert(attempt.candidate_hash.clone());
                    }
                }
                for action in self.actions.values_mut().filter(|a| a.goal_id == goal_id) {
                    if action.finding_ids.iter().any(|id| affected.contains(id)) {
                        action.state = "reopened".into();
                    }
                }
                self.update_completion(&goal_id);
                Ok(
                    json!({"affected_findings":affected,"reopened_needs":reopened,"revision":self.revision}),
                )
            }
            DomainCommand::SubmitAttempt {
                goal_id,
                attempt_id,
                payload,
                finding_ids,
                planning_round_id,
            } => {
                let goal = self.active(&goal_id, now_ms)?.clone();
                ensure!(
                    actor.role == Role::Agent || actor.role == Role::Operator,
                    "proposer required"
                );
                ensure!(
                    valid_id(&attempt_id) && !finding_ids.is_empty(),
                    "Attempt ID and evidence required"
                );
                let (markings, _) = self.sources(&goal_id, &finding_ids)?;
                let candidate_hash = hash_json(&payload)?;
                ensure!(
                    !self
                        .excluded_candidate_hashes
                        .get(&goal_id)
                        .is_some_and(|set| set.contains(&candidate_hash)),
                    "retracted candidate payload cannot repeat"
                );
                if let Some(existing) = self.attempts.get(&key(&goal_id, &attempt_id)) {
                    ensure!(
                        existing.candidate_hash == candidate_hash
                            && existing.finding_ids == finding_ids
                            && existing.producer_id == actor.id
                            && existing.planning_round_id == planning_round_id
                            && self.attempt_eligible(existing),
                        "immutable or retracted Attempt conflict"
                    );
                    return Ok(serde_json::to_value(existing)?);
                }
                let round = self
                    .planning_rounds
                    .get_mut(&key(&goal_id, &planning_round_id))
                    .context("broker-recorded planning round missing")?;
                ensure!(
                    round.goal_version == goal.version
                        && round.producer_id == actor.id
                        && round.introduced.len() < round.candidate_limit
                        && round.candidate_limit <= 8,
                    "candidate exceeds recorded planning round or producer scope"
                );
                round.introduced.insert(attempt_id.clone());
                let attempt = Attempt {
                    correlation: None,
                    id: attempt_id.clone(),
                    goal_id: goal_id.clone(),
                    goal_version: goal.version,
                    producer_id: actor.id.clone(),
                    payload,
                    candidate_hash,
                    finding_ids,
                    markings,
                    flagged: false,
                    recorded_ms: now_ms,
                    scores: vec![],
                    planning_round_id,
                };
                let value = serde_json::to_value(&attempt)?;
                self.attempts.insert(key(&goal_id, &attempt_id), attempt);
                self.goals.get_mut(&goal_id).unwrap().status = "awaiting_evaluation".into();
                Ok(value)
            }
            DomainCommand::ScoreAttempt {
                goal_id,
                attempt_id,
                score,
            } => {
                self.evaluator(actor, &goal_id)?;
                signature(&score.signature)?;
                ensure!(
                    score.evaluator_id == actor.id
                        && score.contract == self.current_contract(&goal_id)?,
                    "score evaluator or contract mismatch"
                );
                ensure!(
                    score.value.is_finite()
                        && (0.0..=1.0).contains(&score.value)
                        && score
                            .dimensions
                            .values()
                            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
                    "score must be finite and normalized"
                );
                let attempt = self.attempt(&goal_id, &attempt_id)?;
                ensure!(
                    self.attempt_eligible(attempt)
                        && score.candidate_hash == attempt.candidate_hash,
                    "score candidate or lineage mismatch"
                );
                let round_id = attempt.planning_round_id.clone();
                let round = self
                    .planning_rounds
                    .get_mut(&key(&goal_id, &round_id))
                    .context("scored Attempt lost planning round")?;
                ensure!(
                    round.introduced.contains(&attempt_id)
                        && (round.evaluated.contains(&attempt_id)
                            || round.evaluated.len() < round.candidate_limit),
                    "evaluation exceeds recorded planning round"
                );
                round.evaluated.insert(attempt_id.clone());
                let attempt = self.attempts.get_mut(&key(&goal_id, &attempt_id)).unwrap();
                if let Some(previous) = attempt.scores.iter().find(|s| s.contract == score.contract)
                {
                    ensure!(
                        hash_json(previous)? == hash_json(&score)?,
                        "immutable score conflict"
                    );
                } else {
                    attempt.scores.push(score);
                }
                Ok(serde_json::to_value(attempt)?)
            }
            DomainCommand::BindActionEvidence { action } => {
                ensure!(
                    actor.role == Role::Operator,
                    "trusted action admission required"
                );
                self.active(&action.goal_id, now_ms)?;
                self.sources(&action.goal_id, &action.finding_ids)?;
                ensure!(
                    action.goal_version == self.goal(&action.goal_id)?.spec.version,
                    "stale action contract"
                );
                ensure!(
                    valid_id(&action.id)
                        && !action.finding_ids.is_empty()
                        && !self.actions.contains_key(&action.id),
                    "invalid evidence action"
                );
                let value = serde_json::to_value(&action)?;
                self.actions.insert(action.id.clone(), action);
                Ok(value)
            }
            DomainCommand::StopGoal { goal_id, reason } => {
                ensure!(
                    actor.role == Role::Operator || actor.role == Role::Policy,
                    "authorized stop actuator required"
                );
                ensure!(!reason.is_empty(), "stop reason required");
                let goal = self.goals.get_mut(&goal_id).context("unknown Goal")?;
                goal.status = "stopped".into();
                goal.stop_reason = Some(reason);
                for task in self
                    .tasks
                    .values_mut()
                    .filter(|t| t.goal_id == goal_id && t.state != TaskState::Completed)
                {
                    task.lease_expiry_ms = 0;
                    task.state = TaskState::Expired;
                }
                Ok(json!({"goal_id":goal_id,"status":"stopped","revision":self.revision}))
            }
            DomainCommand::ResumeGoal { goal_id } => {
                ensure!(
                    matches!(actor.role, Role::Operator | Role::Policy),
                    "authorized release actuator required"
                );
                let goal = self.goals.get_mut(&goal_id).context("unknown Goal")?;
                ensure!(
                    goal.spec.expiry_ms > now_ms && goal.status == "stopped",
                    "Goal is expired or not stopped"
                );
                goal.status = "active".into();
                goal.stop_reason = None;
                self.update_completion(&goal_id);
                Ok(
                    json!({"goal_id":goal_id,"status":self.goal(&goal_id)?.status,"revision":self.revision}),
                )
            }
            DomainCommand::OpenPlanningRound {
                goal_id,
                round_id,
                producer_id,
                context_hash,
                candidate_limit,
            } => {
                let goal = self.active(&goal_id, now_ms)?;
                ensure!(
                    matches!(actor.role, Role::Operator | Role::Policy)
                        && valid_id(&round_id)
                        && !producer_id.is_empty()
                        && (1..=8).contains(&candidate_limit)
                        && context_hash.len() == 64
                        && context_hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "trusted bounded planning round required"
                );
                let goal_version = goal.version;
                if let Some(existing) = self.planning_rounds.get(&key(&goal_id, &round_id)) {
                    ensure!(
                        existing.goal_version == goal_version
                            && existing.producer_id == producer_id
                            && existing.context_hash == context_hash
                            && existing.candidate_limit == candidate_limit,
                        "immutable planning round conflict"
                    );
                    return Ok(serde_json::to_value(existing)?);
                }
                let round = PlanningRound {
                    id: round_id.clone(),
                    goal_id: goal_id.clone(),
                    goal_version,
                    producer_id,
                    context_hash,
                    candidate_limit,
                    introduced: BTreeSet::new(),
                    evaluated: BTreeSet::new(),
                    recorded_ms: now_ms,
                    recorded_revision: self.revision,
                };
                let value = serde_json::to_value(&round)?;
                self.planning_rounds.insert(key(&goal_id, &round_id), round);
                Ok(value)
            }
            DomainCommand::ReadBoard { .. } => bail!("read command must use read-only path"),
        }
    }
    fn owned_task_mut(
        &mut self,
        actor: &Actor,
        goal_id: &str,
        id: &str,
        token: u64,
        now_ms: u64,
    ) -> Result<&mut Task> {
        ensure!(actor.role == Role::Agent, "agent Task holder required");
        let version = self.goal(goal_id)?.spec.version;
        let task = self
            .tasks
            .get_mut(&key(goal_id, id))
            .context("unknown Task")?;
        ensure!(
            task.goal_version == version
                && task.claimed_by.as_deref() == Some(&actor.id)
                && task.fencing_token == token
                && task.lease_expiry_ms > now_ms
                && task.spec.expiry_ms > now_ms,
            "stale or foreign Task holder"
        );
        Ok(task)
    }
}
