//! The words a Goal brings to its run: what the swarm tells an agent that
//! has not filed, and which Task input fields shape the record the host
//! files for one that never did. A Goal that names neither gets the research
//! swarm's, the only ones before a Goal could bring its own.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

const MAX_NUDGE_BYTES: usize = 4096;
const MAX_FIELD_BYTES: usize = 128;
const MAX_FIELDS: usize = 32;
/// The keys a host-filed record sets itself, so no copied field may take one.
const RECORD_KEYS: [&str; 8] = [
    "id",
    "title",
    "file",
    "description",
    "severity",
    "code",
    "output",
    "evidence",
];

const EXPLORE: &str = "\
    You are {agent_id}. Keep researching. Use fetch_url to read the actual sources in your \
    assigned block: the manuscript map, the paper README, and the LaTeX sections under \
    build/sections/. Read several papers before you decide. Post useful intermediates to \
    the Board. Do not file yet unless you have already covered your block and have at \
    least three well-evidenced records. Keep going.";
const ASSEMBLE: &str = "\
    You are {agent_id}. You have enough evidence to begin assembling your records. Post your \
    top candidates to the Board now, then prepare one submit_finding call. Filing becomes \
    mandatory in a few turns; do not open a new line of inquiry.";
const FILE: &str = "\
    You are {agent_id}. Filing is now the ONLY remaining action. Call submit_finding exactly \
    once, now:\n\
    {call}\n\
    Replace the empty list with one object per paper you have already read in this \
    conversation. Each object needs {required_fields}; severity one of {severities}. Every \
    file must be one of your seed URLs. Never invent a paper and never copy this \
    instruction into a record. If you have no defensible record, submit the empty list: an \
    empty finding is recoverable, a missing one blocks the whole Goal. Do not call \
    fetch_url again. After submit_finding returns accepted, call board_post once and end \
    the turn.";
const FILE_WITHOUT_CONTRACT: &str = "\
    You are {agent_id}. Filing is now the ONLY remaining action. Call claim_task if you \
    have not already, then call submit_finding exactly once with your claimed task_id \
    and the result object your worker prompt defines, built from evidence already in \
    this conversation. A filing with no records is better than no filing; never invent \
    a record. Do not call fetch_url again. After submit_finding returns accepted, end \
    the turn.";

/// What the swarm tells an agent whose turn ended before it filed, one text
/// per stage. Each is a template: `{name}` prints a value and a template has
/// no other brace. Every stage has `{agent_id}`; `file` also has `{task_id}`,
/// `{result_key}`, `{required_fields}`, `{severities}` and `{call}`, the exact
/// `submit_finding` call. A stage the Goal leaves out keeps the research text.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Nudges {
    /// The early rounds, while the agent may still explore.
    pub explore: String,
    /// The middle rounds, when it should assemble its records.
    pub assemble: String,
    /// The last rounds, when its claimed Task has a whole filing contract.
    pub file: String,
    /// The last rounds otherwise: no claimed Task, or a contract that lacks
    /// its result key, required fields or severities.
    pub file_without_contract: String,
}

impl Default for Nudges {
    fn default() -> Self {
        Self {
            explore: EXPLORE.into(),
            assemble: ASSEMBLE.into(),
            file: FILE.into(),
            file_without_contract: FILE_WITHOUT_CONTRACT.into(),
        }
    }
}

/// The one filing a claimed Task's `output_contract` admits.
pub struct FilingContract {
    pub task_id: String,
    pub result_key: String,
    /// Joined with `, `.
    pub required_fields: String,
    /// Joined with ` | `.
    pub severities: String,
}

impl Nudges {
    /// What a Goal that names no nudges gets.
    pub fn research() -> &'static Self {
        static RESEARCH: LazyLock<Nudges> = LazyLock::new(Nudges::default);
        &RESEARCH
    }

    pub fn explore(&self, agent_id: &str) -> Result<String> {
        fill(&self.explore, &[("agent_id", agent_id)]).context("nudges.explore")
    }

    pub fn assemble(&self, agent_id: &str) -> Result<String> {
        fill(&self.assemble, &[("agent_id", agent_id)]).context("nudges.assemble")
    }

    pub fn file(&self, agent_id: &str, contract: &FilingContract) -> Result<String> {
        let call = format!(
            "submit_finding({{\"task_id\":\"{}\",\"result\":{{\"{}\":[]}}}})",
            contract.task_id, contract.result_key
        );
        fill(
            &self.file,
            &[
                ("agent_id", agent_id),
                ("task_id", &contract.task_id),
                ("result_key", &contract.result_key),
                ("required_fields", &contract.required_fields),
                ("severities", &contract.severities),
                ("call", &call),
            ],
        )
        .context("nudges.file")
    }

    pub fn file_without_contract(&self, agent_id: &str) -> Result<String> {
        fill(&self.file_without_contract, &[("agent_id", agent_id)])
            .context("nudges.file_without_contract")
    }

    /// Every stage renders, so a nudge cannot fail in the middle of a run.
    pub fn validate(&self) -> Result<()> {
        for (stage, text) in [
            ("explore", &self.explore),
            ("assemble", &self.assemble),
            ("file", &self.file),
            ("file_without_contract", &self.file_without_contract),
        ] {
            ensure!(
                !text.trim().is_empty() && text.len() <= MAX_NUDGE_BYTES,
                "nudges.{stage} must be 1..={MAX_NUDGE_BYTES} bytes and not blank"
            );
        }
        let sample = FilingContract {
            task_id: "task".into(),
            result_key: "records".into(),
            required_fields: "id".into(),
            severities: "high".into(),
        };
        self.explore("agent-00")?;
        self.assemble("agent-00")?;
        self.file("agent-00", &sample)?;
        self.file_without_contract("agent-00")?;
        Ok(())
    }
}

/// `template` with each `{name}` replaced by its value in `values`.
fn fill(template: &str, values: &[(&str, &str)]) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find(['{', '}']) {
        let (text, brace) = rest.split_at(at);
        out.push_str(text);
        let Some(open) = brace.strip_prefix('{') else {
            bail!("a `}}` closes no placeholder; a nudge has no other brace");
        };
        let (name, after) = open
            .split_once('}')
            .context("a `{` opens a placeholder that never closes")?;
        let Some((_, value)) = values.iter().find(|(known, _)| *known == name) else {
            let known: Vec<String> = values.iter().map(|(n, _)| format!("{{{n}}}")).collect();
            bail!(
                "{{{name}}} is not a placeholder here; this one has {}",
                known.join(", ")
            );
        };
        out.push_str(value);
        rest = after;
    }
    out.push_str(rest);
    Ok(out)
}

/// Which Task input fields shape the record the host files for an agent
/// that never filed. A Task whose `output_contract` names no `result_key`
/// files under `marked_result_key` when its input has any of `marked_by`,
/// and under `result_key` otherwise. A field the Goal leaves out keeps the
/// research swarm's.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordFields {
    /// Tried in order for the record's `file`: the first that is a string.
    pub file_from: Vec<String>,
    /// Copied into the record as they are, when the input has them.
    pub copy: Vec<String>,
    pub result_key: String,
    pub marked_result_key: String,
    pub marked_by: Vec<String>,
}

impl Default for RecordFields {
    fn default() -> Self {
        let names = |names: &[&str]| names.iter().map(|&name| name.to_owned()).collect();
        Self {
            file_from: names(&["seed_url", "paper_url", "artifact_file"]),
            copy: names(&["experiment", "component", "compute"]),
            result_key: "inconsistencies".into(),
            marked_result_key: "experiments".into(),
            marked_by: names(&["experiment", "artifact_fields"]),
        }
    }
}

impl RecordFields {
    /// What a Goal that names no record fields gets.
    pub fn research() -> &'static Self {
        static RESEARCH: LazyLock<RecordFields> = LazyLock::new(RecordFields::default);
        &RESEARCH
    }

    pub fn validate(&self) -> Result<()> {
        for (list, names) in [
            ("file_from", &self.file_from),
            ("copy", &self.copy),
            ("marked_by", &self.marked_by),
        ] {
            ensure!(
                names.len() <= MAX_FIELDS,
                "record_fields.{list} names more than {MAX_FIELDS} fields"
            );
            for name in names {
                field_name(name).with_context(|| format!("record_fields.{list}"))?;
            }
        }
        field_name(&self.result_key).context("record_fields.result_key")?;
        field_name(&self.marked_result_key).context("record_fields.marked_result_key")?;
        if let Some(name) = self
            .copy
            .iter()
            .find(|name| RECORD_KEYS.contains(&name.as_str()))
        {
            bail!("record_fields.copy names {name}, which a filed record sets itself");
        }
        Ok(())
    }
}

fn field_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= MAX_FIELD_BYTES,
        "a field name must be 1..={MAX_FIELD_BYTES} bytes"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FilingContract, Nudges, RecordFields, fill};

    fn contract() -> FilingContract {
        FilingContract {
            task_id: "lens-04".into(),
            result_key: "papers".into(),
            required_fields: "id, title".into(),
            severities: "useful | background".into(),
        }
    }

    #[test]
    fn a_placeholder_prints_its_value_and_a_value_is_never_read_as_a_template() {
        assert_eq!(
            fill("{a} and {b}{a}", &[("a", "x"), ("b", "{a}")]).unwrap(),
            "x and {a}x"
        );
        assert_eq!(fill("no braces", &[]).unwrap(), "no braces");
        let own: Nudges = serde_json::from_value(serde_json::json!({
            "file": "{agent_id} files {task_id} under {result_key}: {call} ({required_fields}; {severities})",
        }))
        .unwrap();
        own.validate().unwrap();
        assert_eq!(
            own.file("agent-02", &contract()).unwrap(),
            r#"agent-02 files lens-04 under papers: submit_finding({"task_id":"lens-04","result":{"papers":[]}}) (id, title; useful | background)"#
        );
        assert_eq!(own.explore, Nudges::default().explore, "a stage left out");
    }

    #[test]
    fn a_malformed_nudge_is_refused_with_why() {
        for (case, nudges, why) in [
            (
                "an unknown name",
                serde_json::json!({"explore": "You are {agent}."}),
                "{agent} is not a placeholder here; this one has {agent_id}",
            ),
            (
                "a task placeholder where there is no Task",
                serde_json::json!({"file_without_contract": "File {task_id}."}),
                "{task_id} is not a placeholder",
            ),
            (
                "an open brace that never closes",
                serde_json::json!({"assemble": "You are {agent_id"}),
                "never closes",
            ),
            (
                "a literal brace",
                serde_json::json!({"file": "Send {\"a\":1}"}),
                "is not a placeholder",
            ),
            (
                "a stray closing brace",
                serde_json::json!({"explore": "You are agent}"}),
                "closes no placeholder",
            ),
            (
                "a blank stage",
                serde_json::json!({"assemble": "  "}),
                "not blank",
            ),
            (
                "a stage too long",
                serde_json::json!({"explore": "x".repeat(4097)}),
                "1..=4096 bytes",
            ),
        ] {
            let nudges: Nudges = serde_json::from_value(nudges).unwrap();
            let Err(err) = nudges.validate() else {
                panic!("{case}: accepted")
            };
            assert!(format!("{err:#}").contains(why), "{case}: {err:#}");
        }
        assert!(
            serde_json::from_value::<Nudges>(serde_json::json!({"explor": "x"})).is_err(),
            "an unknown stage"
        );
        Nudges::research().validate().unwrap();
    }

    #[test]
    fn record_fields_refuse_a_blank_name_and_a_key_the_record_sets() {
        RecordFields::research().validate().unwrap();
        for (fields, why) in [
            (
                serde_json::json!({"copy": ["section", "id"]}),
                "copy names id",
            ),
            (
                serde_json::json!({"file_from": [""]}),
                "record_fields.file_from",
            ),
            (
                serde_json::json!({"result_key": ""}),
                "record_fields.result_key",
            ),
            (
                serde_json::json!({"marked_by": vec!["f"; 33]}),
                "more than 32 fields",
            ),
        ] {
            let fields: RecordFields = serde_json::from_value(fields).unwrap();
            let Err(err) = fields.validate() else {
                panic!("accepted {fields:?}")
            };
            assert!(format!("{err:#}").contains(why), "{err:#}");
        }
        assert!(serde_json::from_value::<RecordFields>(serde_json::json!({"files": []})).is_err());
    }
}
