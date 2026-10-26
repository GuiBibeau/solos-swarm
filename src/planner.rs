use crate::domain::{Attempt, DomainState};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    #[default]
    ScoredHistory,
    SingleDraftRefinement,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryConfig {
    #[serde(default)]
    pub mode: HistoryMode,
    pub top_k: usize,
    pub new_candidates: usize,
    pub context_token_budget: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextLimits {
    pub fixed_tokens: usize,
    pub output_tokens: usize,
    pub model_limit: usize,
}
impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            mode: HistoryMode::ScoredHistory,
            top_k: 20,
            new_candidates: 8,
            context_token_budget: 0,
        }
    }
}

/// Counts include formatting and references, measured by the selected model tokenizer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenText {
    pub text: String,
    pub tokens: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextItem {
    pub body: TokenText,
    pub summary: Option<TokenText>,
    pub retraction_marker: TokenText,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HistoryMaterial {
    pub items: BTreeMap<String, ContextItem>,
    pub older_summary: Option<TokenText>,
    pub retractions_summary: Option<TokenText>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryPart {
    pub attempt_id: Option<String>,
    pub kind: String,
    pub content: TokenText,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryContext {
    pub parts: Vec<HistoryPart>,
    pub used_tokens: usize,
    pub fixed_tokens: usize,
    pub output_tokens: usize,
    pub token_limit: usize,
    pub archive_count: usize,
    pub excluded_count: usize,
    pub candidate_limit: usize,
}

fn measured(text: &TokenText) -> Result<()> {
    ensure!(
        !text.text.is_empty() && text.tokens > 0,
        "missing tokenizer-measured context text"
    );
    Ok(())
}

pub struct HistorySelection<'a> {
    pub archive: Vec<&'a Attempt>,
    pub selected: Vec<&'a Attempt>,
    pub excluded: Vec<&'a Attempt>,
}

pub fn select_history<'a>(
    state: &'a DomainState,
    goal_id: &str,
    clearance: &BTreeSet<String>,
    config: &HistoryConfig,
) -> Result<HistorySelection<'a>> {
    let contract = state.current_contract(goal_id)?;
    let archive: Vec<_> = state
        .attempts
        .values()
        .filter(|a| a.goal_id == goal_id && a.markings.is_subset(clearance))
        .collect();
    let mut eligible: Vec<_> = archive
        .iter()
        .copied()
        .filter(|a| state.attempt_eligible(a))
        .collect();
    let excluded: Vec<_> = archive
        .iter()
        .copied()
        .filter(|a| !state.attempt_eligible(a))
        .collect();
    let score = |attempt: &Attempt| {
        attempt
            .scores
            .iter()
            .find(|s| s.contract == contract)
            .map(|s| s.value)
    };
    if config.mode == HistoryMode::SingleDraftRefinement {
        ensure!(
            config.new_candidates == 1,
            "single-draft refinement introduces at most one candidate per round"
        );
        eligible.sort_by(|a, b| {
            b.recorded_ms
                .cmp(&a.recorded_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        let selected = eligible.into_iter().take(1).collect();
        return Ok(HistorySelection {
            archive,
            selected,
            excluded,
        });
    }
    let mut selected: Vec<_> = eligible
        .iter()
        .copied()
        .filter(|a| score(a).is_some())
        .collect();
    selected.sort_by(|a, b| {
        score(b)
            .unwrap()
            .total_cmp(&score(a).unwrap())
            .then_with(|| b.recorded_ms.cmp(&a.recorded_ms))
            .then_with(|| a.id.cmp(&b.id))
    });
    selected.truncate(config.top_k);
    selected.reverse();
    eligible.sort_by(|a, b| {
        b.recorded_ms
            .cmp(&a.recorded_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut ids: BTreeSet<_> = selected.iter().map(|a| a.id.clone()).collect();
    for attempt in eligible.iter().take(config.new_candidates) {
        if ids.insert(attempt.id.clone()) {
            selected.push(attempt);
        }
    }
    Ok(HistorySelection {
        archive,
        selected,
        excluded,
    })
}

/// The caller tokenizes the complete Goal/tools/security contract before reserving spend.
pub fn assemble_history(
    state: &DomainState,
    goal_id: &str,
    clearance: &BTreeSet<String>,
    config: &HistoryConfig,
    limits: &ContextLimits,
    material: &HistoryMaterial,
) -> Result<HistoryContext> {
    let ContextLimits {
        fixed_tokens,
        output_tokens,
        model_limit,
    } = *limits;
    state.goal(goal_id)?;
    ensure!(
        config.top_k > 0
            && config.new_candidates > 0
            && config.context_token_budget > 0
            && model_limit > 0,
        "explicit history limits required"
    );
    let token_limit = config.context_token_budget.min(model_limit);
    let fixed = fixed_tokens
        .checked_add(output_tokens)
        .context("fixed context token count overflow")?;
    ensure!(
        fixed <= token_limit,
        "fixed Goal/security/rubric/tools/output exceed selected model context"
    );
    let HistorySelection {
        archive,
        selected,
        excluded,
    } = select_history(state, goal_id, clearance, config)?;
    let mut context = HistoryContext {
        parts: vec![],
        used_tokens: fixed,
        fixed_tokens,
        output_tokens,
        token_limit,
        archive_count: archive.len(),
        excluded_count: excluded.len(),
        candidate_limit: config.new_candidates,
    };
    let mut remaining = token_limit - fixed;

    // Reserve bounded aggregate markers first, so a long archive never forces overflow.
    if !excluded.is_empty() {
        let summary = material
            .retractions_summary
            .as_ref()
            .context("bounded retraction summary required")?;
        measured(summary)?;
        ensure!(
            summary.tokens <= remaining,
            "retraction summary exceeds working context"
        );
        context.parts.push(HistoryPart {
            attempt_id: None,
            kind: "retraction_summary".into(),
            content: summary.clone(),
        });
        context.used_tokens += summary.tokens;
        remaining -= summary.tokens;
    }
    let selected_tokens = selected
        .iter()
        .try_fold(0usize, |sum, a| {
            material
                .items
                .get(&a.id)
                .and_then(|item| sum.checked_add(item.body.tokens))
        })
        .unwrap_or(usize::MAX);
    if archive.len() > selected.len() + excluded.len() || selected_tokens > remaining {
        let summary = material
            .older_summary
            .as_ref()
            .context("bounded archive summary required")?;
        measured(summary)?;
        ensure!(
            summary.tokens <= remaining,
            "archive summary exceeds working context"
        );
        context.parts.push(HistoryPart {
            attempt_id: None,
            kind: "archive_summary".into(),
            content: summary.clone(),
        });
        context.used_tokens += summary.tokens;
        remaining -= summary.tokens;
    }
    let mut omitted = 0;
    for attempt in selected {
        let item = material
            .items
            .get(&attempt.id)
            .context("missing tokenized Attempt body")?;
        measured(&item.body)?;
        let (content, kind) = if item.body.tokens <= remaining {
            (&item.body, "attempt")
        } else if let Some(summary) = &item.summary {
            measured(summary)?;
            if summary.tokens <= remaining {
                (summary, "attempt_summary")
            } else {
                omitted += 1;
                continue;
            }
        } else {
            omitted += 1;
            continue;
        };
        context.parts.push(HistoryPart {
            attempt_id: Some(attempt.id.clone()),
            kind: kind.into(),
            content: content.clone(),
        });
        context.used_tokens += content.tokens;
        remaining -= content.tokens;
    }
    if omitted > 0 && !context.parts.iter().any(|p| p.kind == "archive_summary") {
        let summary = material
            .older_summary
            .as_ref()
            .context("omitted Attempt aggregate summary required")?;
        measured(summary)?;
        ensure!(
            summary.tokens <= remaining,
            "omitted Attempt summary exceeds working context"
        );
        context.parts.push(HistoryPart {
            attempt_id: None,
            kind: "archive_summary".into(),
            content: summary.clone(),
        });
        context.used_tokens += summary.tokens;
        remaining -= summary.tokens;
    }
    for attempt in excluded {
        let item = material
            .items
            .get(&attempt.id)
            .context("missing retraction marker")?;
        measured(&item.retraction_marker)?;
        if item.retraction_marker.tokens <= remaining {
            context.parts.push(HistoryPart {
                attempt_id: Some(attempt.id.clone()),
                kind: "retraction_marker".into(),
                content: item.retraction_marker.clone(),
            });
            context.used_tokens += item.retraction_marker.tokens;
            remaining -= item.retraction_marker.tokens;
        }
    }
    ensure!(
        context.used_tokens <= token_limit,
        "context assembly overflow"
    );
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Actor, DomainCommand, GoalSpec, NeedSpec, Role};
    use serde_json::json;

    fn setup() -> DomainState {
        let mut state = DomainState::new();
        let spec = GoalSpec {
            goal_id: "goal".into(),
            intent: "solve".into(),
            version: 1,
            rubric_version: "1".into(),
            evaluation_set_version: "1".into(),
            evaluator_version: "1".into(),
            evaluator_id: "eval".into(),
            security: json!({"approved":true}),
            capabilities: BTreeSet::from(["read".into()]),
            budget_microusd: 100,
            max_depth: 4,
            max_agents: 500,
            max_open_tasks: 500,
            max_needs: 100,
            expiry_ms: 10000,
            initial_needs: vec![NeedSpec {
                id: "need".into(),
                parent_id: None,
                description: "answer".into(),
                predicate: json!({"complete":true}),
                version: 1,
                required: true,
                required_capabilities: BTreeSet::new(),
            }],
        };
        state
            .execute(
                &Actor {
                    id: "operator".into(),
                    role: Role::Operator,
                },
                DomainCommand::RegisterGoal { spec },
                0,
            )
            .unwrap();
        state
    }
    fn text(tokens: usize) -> TokenText {
        TokenText {
            text: "measured context".into(),
            tokens,
        }
    }
    fn attempt(state: &mut DomainState, id: usize, flagged: bool) -> String {
        let id = format!("attempt-{id}");
        state.attempts.insert(
            format!("goal/{id}"),
            Attempt {
                correlation: None,
                planning_round_id: "test-historical-round".into(),
                id: id.clone(),
                goal_id: "goal".into(),
                goal_version: 1,
                producer_id: "worker".into(),
                payload: json!({"id":id}),
                candidate_hash: id.clone(),
                finding_ids: vec![],
                markings: BTreeSet::new(),
                flagged,
                recorded_ms: 1,
                scores: vec![],
            },
        );
        id
    }
    #[test]
    fn fixed_contract_overflow_fails_even_with_empty_archive() {
        let state = setup();
        let config = HistoryConfig {
            context_token_budget: 100,
            ..Default::default()
        };
        assert!(
            assemble_history(
                &state,
                "goal",
                &BTreeSet::new(),
                &config,
                &ContextLimits {
                    fixed_tokens: 90,
                    output_tokens: 11,
                    model_limit: 100
                },
                &HistoryMaterial::default()
            )
            .is_err()
        );
    }
    #[test]
    fn single_draft_refinement_reads_latest_draft_instead_of_highest_scored_archive() {
        let mut state = setup();
        let old = attempt(&mut state, 1, false);
        let latest = attempt(&mut state, 2, false);
        let contract = state.current_contract("goal").unwrap();
        let first = state.attempts.get_mut(&format!("goal/{old}")).unwrap();
        first.recorded_ms = 1;
        first.scores.push(crate::domain::Score {
            candidate_hash: first.candidate_hash.clone(),
            contract,
            value: 1.0,
            dimensions: BTreeMap::new(),
            evaluator_id: "eval".into(),
            signature: "archived-test-score".into(),
            recorded_ms: 1,
        });
        state
            .attempts
            .get_mut(&format!("goal/{latest}"))
            .unwrap()
            .recorded_ms = 2;
        let mut baseline = HistoryConfig {
            mode: HistoryMode::SingleDraftRefinement,
            top_k: 20,
            new_candidates: 1,
            context_token_budget: 100,
        };
        let selection = select_history(&state, "goal", &BTreeSet::new(), &baseline).unwrap();
        assert_eq!(
            selection
                .selected
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            vec![latest.as_str()]
        );
        assert_eq!(selection.archive.len(), 2);
        baseline.new_candidates = 2;
        assert!(select_history(&state, "goal", &BTreeSet::new(), &baseline).is_err());
    }
    #[test]
    fn huge_bodies_and_many_retraction_markers_stay_bounded_without_archive_loss() {
        let mut state = setup();
        let mut material = HistoryMaterial {
            older_summary: Some(text(5)),
            retractions_summary: Some(text(5)),
            ..Default::default()
        };
        for index in 0..10000 {
            let id = attempt(&mut state, index, index % 2 == 0);
            material.items.insert(
                id,
                ContextItem {
                    body: text(10000),
                    summary: Some(text(5)),
                    retraction_marker: text(5),
                },
            );
        }
        let context = assemble_history(
            &state,
            "goal",
            &BTreeSet::new(),
            &HistoryConfig {
                context_token_budget: 100,
                ..Default::default()
            },
            &ContextLimits {
                fixed_tokens: 20,
                output_tokens: 20,
                model_limit: 128,
            },
            &material,
        )
        .unwrap();
        assert!(context.used_tokens <= 100);
        assert_eq!(context.excluded_count, 5000);
        assert_eq!(state.attempts.len(), 10000);
        assert!(context.parts.iter().any(|p| p.kind == "retraction_summary"));
        assert!(
            context
                .parts
                .iter()
                .filter(|p| p.kind == "attempt_summary")
                .all(
                    |p| !state.attempts[&format!("goal/{}", p.attempt_id.as_ref().unwrap())]
                        .flagged
                )
        );
    }
    #[test]
    fn clearance_and_smaller_model_are_enforced() {
        let mut state = setup();
        let id = attempt(&mut state, 0, false);
        state
            .attempts
            .get_mut(&format!("goal/{id}"))
            .unwrap()
            .markings
            .insert("secret".into());
        let config = HistoryConfig {
            context_token_budget: 100,
            ..Default::default()
        };
        let context = assemble_history(
            &state,
            "goal",
            &BTreeSet::new(),
            &config,
            &ContextLimits {
                fixed_tokens: 10,
                output_tokens: 10,
                model_limit: 100,
            },
            &HistoryMaterial::default(),
        )
        .unwrap();
        assert_eq!(context.archive_count, 0);
        assert!(
            assemble_history(
                &state,
                "goal",
                &BTreeSet::new(),
                &config,
                &ContextLimits {
                    fixed_tokens: 10,
                    output_tokens: 10,
                    model_limit: 19
                },
                &HistoryMaterial::default()
            )
            .is_err()
        );
    }

    #[test]
    fn best_compatible_scores_are_ordered_ascending_and_full_bodies_preferred() {
        let mut state = setup();
        let contract = state.current_contract("goal").unwrap();
        let mut material = HistoryMaterial {
            older_summary: Some(text(5)),
            ..Default::default()
        };
        for (index, value) in [0.2, 0.8, 0.6].into_iter().enumerate() {
            let id = attempt(&mut state, index, false);
            let attempt = state.attempts.get_mut(&format!("goal/{id}")).unwrap();
            attempt.recorded_ms = index as u64;
            attempt.scores.push(crate::domain::Score {
                candidate_hash: attempt.candidate_hash.clone(),
                contract: contract.clone(),
                value,
                dimensions: BTreeMap::new(),
                evaluator_id: "eval".into(),
                signature: "verified".into(),
                recorded_ms: 1,
            });
            material.items.insert(
                id,
                ContextItem {
                    body: text(10),
                    summary: Some(text(2)),
                    retraction_marker: text(2),
                },
            );
        }
        let context = assemble_history(
            &state,
            "goal",
            &BTreeSet::new(),
            &HistoryConfig {
                mode: crate::planner::HistoryMode::ScoredHistory,
                top_k: 2,
                new_candidates: 1,
                context_token_budget: 100,
            },
            &ContextLimits {
                fixed_tokens: 10,
                output_tokens: 10,
                model_limit: 100,
            },
            &material,
        )
        .unwrap();
        let ids: Vec<_> = context
            .parts
            .iter()
            .filter(|p| p.kind == "attempt")
            .map(|p| p.attempt_id.as_deref().unwrap())
            .collect();
        assert_eq!(ids, vec!["attempt-2", "attempt-1"]);
    }
}
