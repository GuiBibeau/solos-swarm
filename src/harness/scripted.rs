//! A harness that needs no Pi, node or bwrap. Each session plays a fixed script
//! against the real broker over its Unix socket with its bearer token, the way
//! the Pi extension (`extensions/broker.ts`) does: one `POST /model` per
//! prompt, then the tool calls the script names for that turn.
use super::{AgentHarness, Observation, STOP_WAIT, Session, SessionEvent, SessionSpec};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::{sync::mpsc, task::JoinHandle};

/// One scripted tool call.
#[derive(Clone, Debug)]
pub enum Call {
    /// `claim_task`. The claimed Task id is kept for `SubmitFinding`.
    ClaimTask,
    /// `submit_finding` for the claimed Task, with this result.
    SubmitFinding(Value),
    /// `read_findings`. The findings are kept for `SubmitArtifact`.
    ReadFindings,
    /// `submit_artifact` with the exact union of the findings last read.
    SubmitArtifact,
    /// Each tool call the turn's model reply asked for, with its arguments,
    /// as Pi would run them.
    Replied,
}

/// `script[k]` is what a session calls on its `k`-th prompt. A prompt past the
/// end makes the model call and no tool call.
pub type Script = Vec<Vec<Call>>;

/// Scripts by session id. A session with no script only calls the model.
pub struct ScriptedHarness {
    scripts: BTreeMap<String, Script>,
    every_turn: Vec<Call>,
}

impl ScriptedHarness {
    pub fn new(scripts: BTreeMap<String, Script>) -> Self {
        Self {
            scripts,
            every_turn: Vec::new(),
        }
    }

    /// Make these calls on every turn no script names, such as every turn of
    /// a long-lived session whose id is not known in advance.
    pub fn every_turn(mut self, calls: Vec<Call>) -> Self {
        self.every_turn = calls;
        self
    }
}

impl AgentHarness for ScriptedHarness {
    type Session = ScriptedSession;

    async fn start(
        &self,
        spec: &SessionSpec,
        events: mpsc::Sender<Observation>,
    ) -> Result<ScriptedSession> {
        let agent = Agent {
            id: spec.id.clone(),
            token: spec.token.clone(),
            model: spec.model.clone(),
            client: reqwest::Client::builder()
                .unix_socket(spec.broker_socket.clone())
                .build()?,
            claimed: None,
            findings: None,
        };
        let script = self.scripts.get(&spec.id).cloned().unwrap_or_default();
        let (prompts, inbox) = mpsc::unbounded_channel();
        let every_turn = self.every_turn.clone();
        Ok(ScriptedSession {
            id: spec.id.clone(),
            prompts,
            task: tokio::spawn(agent.run(script, every_turn, inbox, events)),
        })
    }
}

pub struct ScriptedSession {
    id: String,
    prompts: mpsc::UnboundedSender<String>,
    task: JoinHandle<()>,
}

impl Session for ScriptedSession {
    fn id(&self) -> &str {
        &self.id
    }

    async fn prompt(&mut self, text: &str) -> Result<()> {
        self.prompts
            .send(text.to_owned())
            .context("scripted session has ended")
    }

    async fn stop(&mut self) -> Result<bool> {
        self.task.abort();
        Ok(tokio::time::timeout(STOP_WAIT, &mut self.task)
            .await
            .is_ok())
    }

    async fn join(self) -> usize {
        0
    }
}

struct Agent {
    id: String,
    token: String,
    model: String,
    client: reqwest::Client,
    claimed: Option<String>,
    findings: Option<Value>,
}

impl Agent {
    /// Play one turn per prompt, in order, until the session is stopped. A
    /// failed call ends the stream with that error.
    async fn run(
        mut self,
        script: Script,
        every_turn: Vec<Call>,
        mut inbox: mpsc::UnboundedReceiver<String>,
        events: mpsc::Sender<Observation>,
    ) {
        let mut turns = script.into_iter();
        while let Some(prompt) = inbox.recv().await {
            let calls = turns.next().unwrap_or_else(|| every_turn.clone());
            if let Err(error) = self.turn(&prompt, &calls).await {
                events.send((self.id, Err(error))).await.ok();
                return;
            }
            events
                .send((self.id.clone(), Ok(SessionEvent::TurnEnded)))
                .await
                .ok();
        }
        events
            .send((self.id, Err(anyhow::anyhow!("scripted session ended"))))
            .await
            .ok();
    }

    async fn turn(&mut self, prompt: &str, calls: &[Call]) -> Result<()> {
        let reply = self
            .post(
                "model",
                json!({"model":self.model,"messages":[{"role":"user","content":prompt}]}),
            )
            .await?;
        for call in calls {
            match call {
                Call::Replied => {
                    let asked = reply["choices"][0]["message"]["tool_calls"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    for tool in asked {
                        let name = tool["function"]["name"]
                            .as_str()
                            .context("a tool call names no tool")?;
                        let arguments = tool["function"]["arguments"]
                            .as_str()
                            .context("a tool call has no arguments")?;
                        let body: Value = serde_json::from_str(arguments)?;
                        self.post(&format!("tool/{name}"), body).await?;
                    }
                }
                Call::ClaimTask => {
                    let task = self.post("tool/claim_task", json!({})).await?;
                    let id = task["id"].as_str().context("claim_task returned no Task")?;
                    self.claimed = Some(id.to_owned());
                }
                Call::SubmitFinding(result) => {
                    let task_id = self.claimed.as_deref().context("no claimed Task")?;
                    let body = json!({"task_id":task_id,"result":result});
                    self.post("tool/submit_finding", body).await?;
                }
                Call::ReadFindings => {
                    self.findings = Some(self.post("tool/read_findings", json!({})).await?);
                }
                Call::SubmitArtifact => {
                    let findings = self.findings.as_ref().context("no findings read")?;
                    let body = json!({"artifact":union(findings)?});
                    let reply = self.post("tool/submit_artifact", body).await?;
                    ensure!(reply["accepted"] == true, "artifact refused: {reply}");
                }
            }
        }
        Ok(())
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let response = self
            .client
            .post(format!("http://broker/{path}"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        ensure!(
            status.is_success(),
            "broker refused {path}: {status} {text}"
        );
        Ok(serde_json::from_str(&text)?)
    }
}

/// Every record of every finding, verbatim, under their one result key.
fn union(findings: &Value) -> Result<Value> {
    let mut key = None;
    let mut records = Vec::new();
    let findings = findings["findings"]
        .as_array()
        .context("no findings list")?;
    for finding in findings {
        let result = finding["result"]
            .as_object()
            .context("result not an object")?;
        for (name, rows) in result {
            key.get_or_insert_with(|| name.clone());
            records.extend(
                rows.as_array()
                    .context("result not a list")?
                    .iter()
                    .cloned(),
            );
        }
    }
    let key: String = key.context("no findings to unite")?;
    Ok(json!({ key: records }))
}
