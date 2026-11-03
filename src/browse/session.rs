use super::cdp::{Cdp, CdpError, Event};
use super::lightpanda::{self, Process};
use super::snapshot::{self, Capture, Element, Login, Text};
use super::{
    ACTION_WAIT, Blob, BlobKind, BrowseCall, Browsed, BrowserConfig, Launcher, NO_SESSION,
    TRAIL_DOMAIN, TrailSigner,
};
use crate::vault::policy::site_listed;
use crate::vault::{Admission, LoginFill, Origin, SecretName, UseContext, Vault};
use anyhow::{Context, Result};
use reqwest::Url;
use serde_json::{Value, json};
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, ElementSeen, MAX_WHY_CHARS, PageAction, PageSeen, Provenance,
    STEP_VERSION, Signature, SignedStep, StepBody, StepOutcome, TrailAction,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;

const NAVIGATION_START_WAIT: Duration = Duration::from_millis(500);
const ID_BYTES: usize = 8;

/// Private to this module, so `release` is the only code that signs.
impl TrailSigner {
    fn sign(&self, body: StepBody) -> SignedStep {
        let signature = self
            .0
            .sign(TRAIL_DOMAIN, &body)
            .expect("a step body always serializes")
            .signature;
        SignedStep {
            body,
            signer: self.public(),
            signature: Signature::try_from(super::hex(&signature))
                .expect("an Ed25519 signature is 64 bytes"),
        }
    }
}

pub(super) fn new_id() -> Result<String> {
    let mut bytes = [0u8; ID_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
    Ok(super::hex(&bytes))
}

pub(super) fn id_placeholder() -> String {
    "0".repeat(2 * ID_BYTES)
}

pub(super) fn is_id(name: &str) -> bool {
    name.len() == 2 * ID_BYTES
        && name
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) struct Session {
    id: String,
    next_step: u32,
    last_n: u32,
    provenance: Option<Provenance>,
    process: Option<Process>,
    cdp: Cdp,
    target: String,
    page_session: String,
    main_frame: String,
    view: Vec<Element>,
    filled_passwords: BTreeSet<u64>,
}

pub(super) enum Raw {
    Page(Box<Shown>),
    NeedsApproval { secret: String, label: String },
    Refused(String),
    Closed(CloseReason),
}

pub(super) struct Shown {
    capture: Capture,
    text: Text,
    target: Option<ElementSeen>,
    login: Option<Login>,
    error: Option<String>,
}

impl Shown {
    fn page(capture: Capture) -> Self {
        Self {
            capture,
            text: Text::Part(1),
            target: None,
            login: None,
            error: None,
        }
    }
}

enum Stop {
    Refused(String),
    Gone(String),
}

impl From<CdpError> for Stop {
    fn from(error: CdpError) -> Self {
        match error {
            CdpError::Refused(why) => Self::Refused(format!("the browser refused {why}")),
            CdpError::Gone(why) => Self::Gone(why),
        }
    }
}

fn refuse<T>(why: impl Into<String>) -> Result<T, Stop> {
    Err(Stop::Refused(why.into()))
}

struct Described {
    node_id: Value,
    backend: u64,
    name: String,
    attributes: Vec<(String, String)>,
}

impl Described {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Form {
    node: u64,
    action: Option<String>,
    fields: Vec<(u64, String)>,
}

fn forms(detected: &Value) -> Vec<Form> {
    let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
    list(&detected["forms"])
        .iter()
        .filter_map(|form| {
            Some(Form {
                node: form["backendNodeId"].as_u64()?,
                action: form["action"].as_str().map(str::to_owned),
                fields: list(&form["fields"])
                    .iter()
                    .filter_map(|field| {
                        let kind = field["inputType"].as_str().unwrap_or_default();
                        Some((field["backendNodeId"].as_u64()?, kind.to_ascii_lowercase()))
                    })
                    .collect(),
            })
        })
        .collect()
}

fn posts_to_site(
    page: &Url,
    action: &str,
    fill: &LoginFill,
    name: &SecretName,
) -> Result<(), Stop> {
    let target = page
        .join(action)
        .map_err(|_| Stop::Refused(format!("the form's action {action:?} is not a URL")))?;
    match Origin::of(&target) {
        Ok(origin) if site_listed(&fill.sites, &origin) => Ok(()),
        Ok(origin) => refuse(format!("the form posts to {origin}, not to {name}'s site")),
        Err(why) => refuse(format!("the form posts to {target}: {why}")),
    }
}

impl Session {
    pub(super) async fn start(
        config: &BrowserConfig,
        runtime_dir: &Path,
        launcher: &Launcher,
    ) -> Result<Self> {
        let id = new_id()?;
        let (process, mut cdp, provenance) = match launcher {
            Launcher::Lightpanda => {
                let (process, cdp, provenance) =
                    Process::start(config, &runtime_dir.join(&id)).await?;
                (Some(process), cdp, provenance)
            }
            Launcher::Socket(socket) => {
                let provenance = Provenance {
                    binary_sha256: Digest::of(b""),
                    argv_sha256: lightpanda::argv_digest(&["socket".into(), socket.into()]),
                };
                (None, Cdp::connect(socket).await?, provenance)
            }
        };
        let string = |value: &Value, what: &str| {
            value
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("the browser named no {what}"))
        };
        let target = cdp
            .call(None, "Target.createTarget", json!({"url": "about:blank"}))
            .await?;
        let target = string(&target["targetId"], "target")?;
        let page = cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?;
        let page = string(&page["sessionId"], "session")?;
        cdp.call(Some(&page), "Page.enable", json!({})).await?;
        let frames = cdp
            .call(Some(&page), "Page.getFrameTree", json!({}))
            .await?;
        let main_frame = string(&frames["frameTree"]["frame"]["id"], "main frame")?;
        Ok(Self {
            id,
            next_step: 0,
            last_n: 0,
            provenance: Some(provenance),
            process,
            cdp,
            target,
            page_session: page,
            main_frame,
            view: Vec::new(),
            filled_passwords: BTreeSet::new(),
        })
    }

    pub(super) async fn end(self) {
        let Self { cdp, process, .. } = self;
        drop(cdp);
        if let Some(process) = process {
            process.end().await;
        }
    }

    pub(super) async fn step(&mut self, vault: Option<&Vault>, call: &BrowseCall) -> Raw {
        if call.n <= self.last_n {
            return Raw::Refused(format!("step {} is not after step {}", call.n, self.last_n));
        }
        self.last_n = call.n;
        let done = match &call.action {
            TrailAction::Open { url, key } => self.open(url, key.is_some()).await,
            TrailAction::Act {
                action,
                element,
                text,
            } => {
                self.act(*action, *element, text.as_ref(), call.typed.as_deref())
                    .await
            }
            TrailAction::Login {
                login,
                username_element,
                password_element,
                submit,
            } => {
                self.login(
                    vault,
                    login,
                    *username_element,
                    *password_element,
                    *submit,
                    &call.context,
                )
                .await
            }
            TrailAction::Read { part, element } => self.read(*part, *element).await,
            TrailAction::Close { reason } => {
                let target = json!({"targetId": self.target});
                let _ = self.cdp.call(None, "Target.closeTarget", target).await;
                Ok(Raw::Closed(*reason))
            }
        };
        match done {
            Ok(raw) => raw,
            Err(Stop::Refused(why)) => Raw::Refused(why),
            Err(Stop::Gone(why)) => {
                eprintln!("browse: session {} stopped: {why}", self.id);
                Raw::Closed(CloseReason::Crashed)
            }
        }
    }

    async fn call(&mut self, method: &'static str, params: Value) -> Result<Value, Stop> {
        Ok(self
            .cdp
            .call(Some(&self.page_session), method, params)
            .await?)
    }

    async fn capture(&mut self) -> Result<Capture, Stop> {
        Ok(snapshot::capture(
            &mut self.cdp,
            &self.page_session,
            &self.target,
            &self.filled_passwords,
        )
        .await?)
    }

    fn element(&self, id: u32) -> Result<Element, Stop> {
        id.checked_sub(1)
            .and_then(|at| self.view.get(usize::try_from(at).ok()?))
            .cloned()
            .ok_or_else(|| Stop::Refused(format!("element {id} is not in the latest view")))
    }

    async fn settle(&mut self, navigating: bool) {
        let mut started = navigating;
        let mut deadline = Instant::now()
            + if navigating {
                ACTION_WAIT
            } else {
                NAVIGATION_START_WAIT
            };
        while let Some(event) = self.cdp.event_until(deadline).await {
            match event {
                Event::Started(frame) if frame == self.main_frame && !started => {
                    started = true;
                    deadline = Instant::now() + ACTION_WAIT;
                }
                Event::Stopped(frame) if frame == self.main_frame && started => return,
                _ => {}
            }
        }
    }

    async fn main_url(&mut self) -> Result<Url, Stop> {
        let frames = self.call("Page.getFrameTree", json!({})).await?;
        let url = frames["frameTree"]["frame"]["url"]
            .as_str()
            .unwrap_or_default();
        Url::parse(url)
            .map_err(|_| Stop::Refused(format!("the page's address {url:?} is not a URL")))
    }

    async fn describe(&mut self, node: Value) -> Result<Described, Stop> {
        let described = self.call("DOM.describeNode", node).await?;
        let node = &described["node"];
        let attributes = node["attributes"]
            .as_array()
            .map(|list| {
                list.chunks(2)
                    .filter_map(|pair| {
                        Some((
                            pair.first()?.as_str()?.to_owned(),
                            pair.get(1)?.as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Described {
            node_id: node["nodeId"].clone(),
            backend: node["backendNodeId"].as_u64().unwrap_or_default(),
            name: node["nodeName"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_uppercase(),
            attributes,
        })
    }

    async fn select(&mut self, node_id: Value, selector: &'static str) -> Result<Vec<Value>, Stop> {
        let found = self
            .call(
                "DOM.querySelectorAll",
                json!({"nodeId": node_id, "selector": selector}),
            )
            .await?;
        Ok(found["nodeIds"].as_array().cloned().unwrap_or_default())
    }

    async fn open(&mut self, url: &str, keyed: bool) -> Result<Raw, Stop> {
        if keyed {
            return refuse("this browser does not add API keys yet; open the page without a key");
        }
        let Some(address) = Url::parse(url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        else {
            return refuse(format!("{url} is not an http or https URL"));
        };
        self.cdp.clear_events();
        let navigated = self
            .call("Page.navigate", json!({"url": address.as_str()}))
            .await?;
        self.settle(true).await;
        let mut shown = Shown::page(self.capture().await?);
        shown.error = navigated["errorText"].as_str().map(str::to_owned);
        Ok(Raw::Page(Box::new(shown)))
    }

    async fn act(
        &mut self,
        action: PageAction,
        element: Option<u32>,
        text: Option<&ActText>,
        typed: Option<&str>,
    ) -> Result<Raw, Stop> {
        let verb = format!("{action:?}").to_lowercase();
        let seen = element.map(|id| self.element(id)).transpose()?;
        let node = seen.as_ref().map(|seen| seen.node);
        let needs_element = || Stop::Refused(format!("{verb} needs an element"));
        let named = match text {
            Some(ActText::Named { text }) => Some(text.as_str()),
            _ => None,
        };
        let needs_text = || Stop::Refused(format!("{verb} needs its text"));
        self.cdp.clear_events();
        let mut navigating = false;
        match action {
            PageAction::Click => {
                let node = node.ok_or_else(needs_element)?;
                self.call("LP.clickNode", json!({"backendNodeId": node}))
                    .await?;
            }
            PageAction::Type => {
                let node = node.ok_or_else(needs_element)?;
                if seen.as_ref().is_some_and(|seen| seen.secret) {
                    return refuse(format!(
                        "element {} is a password field; browser_login fills passwords",
                        element.unwrap_or_default()
                    ));
                }
                let typed = typed.ok_or_else(needs_text)?;
                self.call("DOM.focus", json!({"backendNodeId": node}))
                    .await?;
                self.call("Input.insertText", json!({"text": typed}))
                    .await?;
            }
            PageAction::Select => {
                let node = node.ok_or_else(needs_element)?;
                let label = named.ok_or_else(needs_text)?;
                let details = self
                    .call("LP.getNodeDetails", json!({"backendNodeId": node}))
                    .await?;
                let value = details["nodeDetails"]["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|option| {
                        option["text"].as_str() == Some(label)
                            || option["value"].as_str() == Some(label)
                    })
                    .and_then(|option| option["value"].as_str())
                    .map(str::to_owned);
                let Some(value) = value else {
                    return refuse(format!(
                        "element {} has no option {label:?}",
                        element.unwrap_or_default()
                    ));
                };
                self.call("LP.fillNode", json!({"backendNodeId": node, "text": value}))
                    .await?;
            }
            PageAction::Press => {
                let key = named.ok_or_else(needs_text)?;
                if let Some(node) = node {
                    self.call("DOM.focus", json!({"backendNodeId": node}))
                        .await?;
                }
                for kind in ["keyDown", "keyUp"] {
                    self.call("Input.dispatchKeyEvent", json!({"type": kind, "key": key}))
                        .await?;
                }
            }
            PageAction::Scroll => {
                let delta = match named.ok_or_else(needs_text)? {
                    "down" => 600,
                    "up" => -600,
                    _ => return refuse("scroll goes up or down"),
                };
                match node {
                    Some(node) => {
                        self.call("LP.scrollNode", json!({"backendNodeId": node}))
                            .await?
                    }
                    None => {
                        let wheel = json!({"type": "mouseWheel", "x": 0, "y": 0, "deltaX": 0, "deltaY": delta});
                        self.call("Input.dispatchMouseEvent", wheel).await?
                    }
                };
            }
            PageAction::Back => {
                let history = self.call("Page.getNavigationHistory", json!({})).await?;
                let previous = history["currentIndex"]
                    .as_u64()
                    .and_then(|at| at.checked_sub(1))
                    .and_then(|at| history["entries"].get(usize::try_from(at).ok()?))
                    .and_then(|entry| entry["id"].as_u64());
                let Some(previous) = previous else {
                    return refuse("there is no earlier page");
                };
                self.call("Page.navigateToHistoryEntry", json!({"entryId": previous}))
                    .await?;
                navigating = true;
            }
        }
        self.settle(navigating).await;
        let mut shown = Shown::page(self.capture().await?);
        shown.target = seen.map(|seen| ElementSeen {
            role: seen.role,
            name: seen.name,
        });
        Ok(Raw::Page(Box::new(shown)))
    }

    async fn login(
        &mut self,
        vault: Option<&Vault>,
        login: &str,
        username: Option<u32>,
        password: Option<u32>,
        submit: bool,
        context: &UseContext,
    ) -> Result<Raw, Stop> {
        let Some(vault) = vault else {
            return refuse("there is no vault on this machine");
        };
        let name = SecretName::try_from(login.to_owned()).map_err(Stop::Refused)?;
        let page = self.main_url().await?;
        let origin = Origin::of(&page).map_err(Stop::Refused)?;
        let fill = match vault.admit_login(&name, &origin, context) {
            Err(_) => return refuse("this machine's vault did not open"),
            Ok(Admission::Refused(why)) => return refuse(why),
            Ok(Admission::NeedsApproval { secret, label, .. }) => {
                return Ok(Raw::NeedsApproval { secret, label });
            }
            Ok(Admission::Allowed(fill)) => fill,
        };
        let forms = forms(&self.call("LP.detectForms", json!({})).await?);
        let field = password.map_or_else(
            || "the password field".to_owned(),
            |id| format!("element {id}"),
        );
        let password_node = match password {
            Some(id) => self.element(id)?.node,
            None => forms
                .iter()
                .flat_map(|form| &form.fields)
                .find(|(_, kind)| kind == "password")
                .map(|(node, _)| *node)
                .ok_or_else(|| Stop::Refused("this page shows no password field".into()))?,
        };
        let document = self.call("DOM.getDocument", json!({"depth": 0})).await?;
        let mut in_main_document = false;
        for found in self
            .select(document["root"]["nodeId"].clone(), "input[type=password i]")
            .await?
        {
            in_main_document |=
                self.describe(json!({"nodeId": found})).await?.backend == password_node;
        }
        if !in_main_document {
            return refuse(format!(
                "{field} is not a password input of this page's main document"
            ));
        }
        let Some(form) = forms
            .iter()
            .find(|form| form.fields.iter().any(|(node, _)| *node == password_node))
        else {
            return refuse(format!("{field} is in no form"));
        };
        let username_node = match username {
            Some(id) => {
                let node = self.element(id)?.node;
                let described = self.describe(json!({"backendNodeId": node})).await?;
                let kind = described
                    .attribute("type")
                    .unwrap_or("text")
                    .to_ascii_lowercase();
                if described.name != "INPUT" || !matches!(kind.as_str(), "text" | "email" | "tel") {
                    return refuse(format!("element {id} is not a text, email or tel input"));
                }
                Some(node)
            }
            None => form
                .fields
                .iter()
                .find(|(_, kind)| matches!(kind.as_str(), "text" | "email" | "tel"))
                .map(|(node, _)| *node),
        };
        posts_to_site(
            &page,
            form.action.as_deref().unwrap_or_default(),
            &fill,
            &name,
        )?;
        let controls = self.submit_controls(form.node).await?;
        for (_, action) in &controls {
            if let Some(action) = action {
                posts_to_site(&page, action, &fill, &name)?;
            }
        }
        let control = controls.first().filter(|_| submit).map(|(node, _)| *node);
        let now = self.main_url().await?;
        if Origin::of(&now).ok().as_ref() != Some(&origin) {
            return refuse(format!("the page moved to {now} before the fill"));
        }
        let mut filled = Vec::new();
        if let Some(node) = username_node {
            self.call(
                "LP.fillNode",
                json!({"backendNodeId": node, "text": fill.username}),
            )
            .await?;
            filled.push("username".to_owned());
        }
        self.call(
            "LP.fillNode",
            json!({"backendNodeId": password_node, "text": fill.reveal_password()}),
        )
        .await?;
        self.filled_passwords.insert(password_node);
        filled.push("password".to_owned());
        let submitted = match control {
            Some(node) => {
                self.cdp.clear_events();
                self.call("LP.clickNode", json!({"backendNodeId": node}))
                    .await?;
                self.settle(false).await;
                true
            }
            None => false,
        };
        let mut shown = Shown::page(self.capture().await?);
        shown.login = Some(Login { filled, submitted });
        Ok(Raw::Page(Box::new(shown)))
    }

    async fn submit_controls(&mut self, form: u64) -> Result<Vec<(u64, Option<String>)>, Stop> {
        let form = self.describe(json!({"backendNodeId": form})).await?;
        let mut found = self
            .select(
                form.node_id.clone(),
                "button, input[type=submit], input[type=image]",
            )
            .await?;
        let named = form
            .attribute("id")
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if named.is_some() {
            let document = self.call("DOM.getDocument", json!({"depth": 0})).await?;
            found.extend(
                self.select(document["root"]["nodeId"].clone(), "[form]")
                    .await?,
            );
        }
        let mut controls = Vec::new();
        for found in found {
            let control = self.describe(json!({"nodeId": found})).await?;
            let kind = control
                .attribute("type")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let submits = match control.name.as_str() {
                "BUTTON" => kind.is_empty() || kind == "submit",
                "INPUT" => kind == "submit" || kind == "image",
                _ => false,
            };
            let owner = control.attribute("form");
            let ours = owner.is_none() || owner == named.as_deref();
            if submits && ours && !controls.iter().any(|(node, _)| *node == control.backend) {
                controls.push((
                    control.backend,
                    control.attribute("formaction").map(str::to_owned),
                ));
            }
        }
        Ok(controls)
    }

    async fn read(&mut self, part: Option<u32>, element: Option<u32>) -> Result<Raw, Stop> {
        let text = match (part, element) {
            (Some(_), Some(_)) => {
                return refuse("read a part of the page or one element, not both");
            }
            (_, Some(id)) => {
                let node = self.element(id)?.node;
                let described = self.describe(json!({"backendNodeId": node})).await?;
                let markdown = self
                    .call("LP.getMarkdown", json!({"nodeId": described.node_id}))
                    .await?;
                let text = snapshot::unescape(markdown["markdown"].as_str().unwrap_or_default());
                Text::Element { id, text }
            }
            (part, None) => Text::Part(part.unwrap_or(1)),
        };
        let mut shown = Shown::page(self.capture().await?);
        shown.text = text;
        Ok(Raw::Page(Box::new(shown)))
    }
}

pub(super) fn release(
    signer: &TrailSigner,
    environment: &str,
    vault: Option<&Vault>,
    call: &BrowseCall,
    session: Option<&mut Session>,
    raw: Raw,
) -> Browsed {
    let (scrubber, raw) = match vault.map(Vault::scrubber).transpose() {
        Ok(scrubber) => (scrubber, raw),
        Err(_) => {
            let raw = match raw {
                Raw::Closed(reason) => Raw::Closed(reason),
                _ => Raw::Refused(
                    "this machine's vault did not open, so nothing the browser saw can be shown"
                        .into(),
                ),
            };
            (None, raw)
        }
    };
    let clean = |text: &str| match &scrubber {
        Some(scrubber) => scrubber.clean(text),
        None => text.to_owned(),
    };
    let refused = |why: &str| StepOutcome::Refused {
        why: snapshot::chars(&clean(why), MAX_WHY_CHARS),
    };
    let mut blobs = Vec::new();
    let mut shown = None;
    let outcome = match raw {
        Raw::Page(page) => match seen(*page, &clean, vault, call) {
            Ok((seen, page_blobs, elements)) => {
                blobs = page_blobs;
                shown = Some(elements);
                StepOutcome::Page(seen)
            }
            Err(why) => refused(&why),
        },
        Raw::NeedsApproval { secret, label } => StepOutcome::NeedsApproval {
            secret: clean(&secret),
            label: clean(&label),
        },
        Raw::Refused(why) => refused(&why),
        Raw::Closed(reason) => StepOutcome::Closed {
            steps: session.as_ref().map_or(0, |session| session.next_step + 1),
            reason,
        },
    };
    let (id, session_step, provenance) = match session {
        Some(session) => {
            if let Some(elements) = shown {
                session.view = elements;
            }
            let step = session.next_step;
            session.next_step += 1;
            (session.id.clone(), step, session.provenance.take())
        }
        None => (NO_SESSION.to_owned(), 0, None),
    };
    let body = StepBody {
        v: STEP_VERSION,
        ticket: call.ticket.clone(),
        assignment: call.assignment.clone(),
        n: call.n,
        environment: environment.to_owned(),
        session: id,
        session_step,
        at_ms: crate::broker::now_ms(),
        provenance,
        action: call.action.clone(),
        outcome,
    };
    Browsed {
        step: signer.sign(body),
        blobs,
    }
}

fn seen(
    shown: Shown,
    clean: &impl Fn(&str) -> String,
    vault: Option<&Vault>,
    call: &BrowseCall,
) -> Result<(PageSeen, Vec<Blob>, Vec<Element>), String> {
    let Shown {
        mut capture,
        text,
        target,
        login,
        error,
    } = shown;
    let origin = Url::parse(&capture.url)
        .ok()
        .and_then(|url| Origin::of(&url).ok());
    let logins = match origin {
        Some(origin) => super::catalog(vault, &call.context)
            .unwrap_or_default()
            .into_iter()
            .filter(|item| {
                matches!(&item.kind, crate::vault::ItemKind::Login { sites, .. } if site_listed(sites, &origin))
            })
            .map(|item| item.name.to_string())
            .collect(),
        None => Vec::new(),
    };
    capture.scrub(clean);
    let text = match text {
        Text::Element { id, text } => Text::Element {
            id,
            text: clean(&text),
        },
        part => part,
    };
    let error = error.as_deref().map(clean);
    let (view, numbered) = snapshot::view(
        &capture,
        call.n,
        &text,
        logins,
        login.as_ref(),
        error.as_deref(),
    )?;
    let snapshot = serde_json::to_vec(&capture).expect("a capture always serializes");
    let seen = PageSeen {
        url: capture.url.clone(),
        status: capture.status,
        title: capture.title.clone(),
        snapshot: Digest::of(&snapshot),
        view: Digest::of(&view),
        target: target.map(|target| ElementSeen {
            role: clean(&target.role),
            name: clean(&target.name),
        }),
        filled: login.map(|login| login.filled).unwrap_or_default(),
        keyed: Vec::new(),
    };
    let blobs = vec![
        Blob {
            digest: seen.snapshot.clone(),
            kind: BlobKind::Snapshot,
            bytes: snapshot,
        },
        Blob {
            digest: seen.view.clone(),
            kind: BlobKind::View,
            bytes: view,
        },
    ];
    capture.elements.truncate(numbered);
    Ok((seen, blobs, capture.elements))
}
