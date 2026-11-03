//! A CDP server on a Unix socket, for tests. It models pages, forms and
//! inputs, answers the methods `browse` sends with the shapes Lightpanda
//! 1.0.2 answers them with (see `docs/agent-mode/vault-browser/
//! lightpanda-probe.md`), and records every method it receives. Each
//! connection is a browser with one tab.

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

const MAIN_FRAME: &str = "F1";
const DOCUMENT_IDS: u64 = 10_000;
const FORM_IDS: u64 = 1_000;

#[derive(Clone, Debug, Default)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub status: u16,
    pub text: String,
    pub elements: Vec<Element>,
    pub forms: Vec<Form>,
    /// A password filled here, or posted here by a form, is written back
    /// into the page the ways a page can: its text (in several encodings),
    /// its title, its HTML, its URL and an element's name.
    pub echo: bool,
}

#[derive(Clone, Debug)]
pub struct Element {
    pub tag: &'static str,
    pub role: &'static str,
    pub name: String,
    /// An `<input>`'s `type`.
    pub kind: Option<&'static str>,
    /// In a child frame rather than in the main document.
    pub subframe: bool,
    /// Where a click on it navigates.
    pub goes: Option<String>,
    pub formaction: Option<String>,
}

impl Element {
    pub fn input(kind: &'static str, name: &str) -> Self {
        Self {
            tag: "input",
            role: "textbox",
            name: name.to_owned(),
            kind: Some(kind),
            subframe: false,
            goes: None,
            formaction: None,
        }
    }

    pub fn textarea(name: &str) -> Self {
        Self {
            tag: "textarea",
            kind: None,
            ..Self::input("text", name)
        }
    }

    pub fn button(name: &str) -> Self {
        Self {
            tag: "button",
            role: "button",
            kind: None,
            ..Self::input("text", name)
        }
    }

    pub fn link(name: &str, goes: &str) -> Self {
        Self {
            tag: "a",
            role: "link",
            kind: None,
            goes: Some(goes.to_owned()),
            ..Self::input("text", name)
        }
    }

    fn password(&self) -> bool {
        self.tag == "input"
            && self
                .kind
                .is_some_and(|kind| kind.eq_ignore_ascii_case("password"))
    }
}

#[derive(Clone, Debug)]
pub struct Form {
    /// The `action` attribute, as written.
    pub action: Option<String>,
    /// Indexes into the page's elements.
    pub fields: Vec<usize>,
    pub submit: Option<usize>,
}

/// One `LP.fillNode` the fake received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    pub url: String,
    /// The index of the filled element in its page.
    pub element: usize,
    pub text: String,
}

#[derive(Default)]
struct Shared {
    pages: Vec<Page>,
    methods: Vec<String>,
    fills: Vec<Fill>,
    crash_on: Option<&'static str>,
    move_on: Option<Move>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct FakeCdp {
    socket: PathBuf,
    shared: Arc<Mutex<Shared>>,
    accept: JoinHandle<()>,
}

impl Drop for FakeCdp {
    fn drop(&mut self) {
        self.accept.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl FakeCdp {
    /// Listen at `socket`. Call it inside a Tokio runtime.
    pub fn start(socket: &Path, pages: Vec<Page>) -> std::io::Result<Self> {
        let listener = UnixListener::bind(socket)?;
        let shared = Arc::new(Mutex::new(Shared {
            pages,
            ..Shared::default()
        }));
        let accepted = Arc::clone(&shared);
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&accepted)));
            }
        });
        Ok(Self {
            socket: socket.to_owned(),
            shared,
            accept,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Every method received, in order, on every connection.
    pub fn methods(&self) -> Vec<String> {
        lock(&self.shared).methods.clone()
    }

    pub fn fills(&self) -> Vec<Fill> {
        lock(&self.shared).fills.clone()
    }

    /// The next time `method` arrives, drop the connection without an
    /// answer, as a browser that crashed does.
    pub fn crash_on(&self, method: &'static str) {
        lock(&self.shared).crash_on = Some(method);
    }

    /// When `method` arrives after `skip` more of it, move the tab to
    /// `url` before answering, as a page that navigates on its own does.
    pub fn move_on(&self, method: &'static str, skip: usize, url: &str) {
        lock(&self.shared).move_on = Some(Move {
            method,
            skip,
            url: url.to_owned(),
        });
    }
}

struct Move {
    method: &'static str,
    skip: usize,
    url: String,
}

async fn serve(stream: UnixStream, shared: Arc<Mutex<Shared>>) {
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let mut tab = Tab::new();
    while let Some(Ok(message)) = socket.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(request) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let mut out = Outbox::default();
        let reply = {
            let mut shared = lock(&shared);
            shared.methods.push(method.clone());
            if shared.crash_on == Some(method.as_str()) {
                shared.crash_on = None;
                return;
            }
            if let Some(moving) = shared
                .move_on
                .as_mut()
                .filter(|moving| moving.method == method)
            {
                match moving.skip.checked_sub(1) {
                    Some(skip) => moving.skip = skip,
                    None => {
                        let url = moving.url.clone();
                        shared.move_on = None;
                        tab.navigate(&url, &shared, None);
                    }
                }
            }
            match tab.answer(&method, &request["params"], &mut shared, &mut out) {
                Ok(result) => {
                    json!({"id": request["id"], "result": result, "sessionId": request["sessionId"]})
                }
                Err(message) => {
                    json!({"id": request["id"], "error": {"code": -32601, "message": message}})
                }
            }
        };
        let messages = out.before.into_iter().chain([reply]).chain(out.after);
        for message in messages {
            if socket
                .send(Message::text(message.to_string()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

/// Events sent before and after a reply, as Lightpanda orders them.
#[derive(Default)]
struct Outbox {
    before: Vec<Value>,
    after: Vec<Value>,
}

impl Outbox {
    fn navigation(&mut self) {
        self.before.push(
            json!({"method": "Page.frameStartedNavigating", "params": {"frameId": MAIN_FRAME}}),
        );
        self.after
            .push(json!({"method": "Page.frameStoppedLoading", "params": {"frameId": MAIN_FRAME}}));
    }
}

struct Tab {
    page: Page,
    /// Node ids of this document start after it, so ids from an earlier
    /// document never name a node of this one, as in Lightpanda.
    base: u64,
    echoed: Option<String>,
    values: BTreeMap<u64, String>,
    focused: Option<u64>,
    history: Vec<String>,
    at: usize,
}

impl Tab {
    fn new() -> Self {
        Self {
            page: Page {
                url: "about:blank".into(),
                ..Page::default()
            },
            base: DOCUMENT_IDS,
            echoed: None,
            values: BTreeMap::new(),
            focused: None,
            history: vec!["about:blank".into()],
            at: 0,
        }
    }

    fn navigate(&mut self, url: &str, shared: &Shared, posted: Option<String>) {
        self.history.truncate(self.at + 1);
        self.history.push(url.to_owned());
        self.at = self.history.len() - 1;
        self.load(url, shared, posted);
    }

    fn load(&mut self, url: &str, shared: &Shared, posted: Option<String>) {
        self.page = shared
            .pages
            .iter()
            .find(|page| page.url == url)
            .cloned()
            .unwrap_or_else(|| Page {
                url: url.to_owned(),
                ..Page::default()
            });
        self.base += DOCUMENT_IDS;
        self.echoed = posted.filter(|_| self.page.echo);
        self.values.clear();
        self.focused = None;
    }

    fn element_id(&self, index: usize) -> u64 {
        self.base + 1 + index as u64
    }

    fn form_id(&self, index: usize) -> u64 {
        self.base + FORM_IDS + index as u64
    }

    fn element(&self, id: u64) -> Option<(usize, &Element)> {
        let index = usize::try_from(id.checked_sub(self.base + 1)?).ok()?;
        Some((index, self.page.elements.get(index)?))
    }

    fn echo_id(&self) -> u64 {
        self.base + FORM_IDS - 1
    }

    fn form(&self, id: u64) -> Option<&Form> {
        self.page
            .forms
            .get(usize::try_from(id.checked_sub(self.base + FORM_IDS)?).ok()?)
    }

    fn url(&self) -> String {
        match &self.echoed {
            Some(value) => format!("{}?echo={}", self.page.url, percent(value, false)),
            None => self.page.url.clone(),
        }
    }

    fn title(&self) -> String {
        match &self.echoed {
            Some(value) => format!("{} {value}", self.page.title),
            None => self.page.title.clone(),
        }
    }

    fn text(&self) -> String {
        let mut text = self.page.text.clone();
        if let Some(value) = &self.echoed {
            let _ = write!(
                text,
                "\nYou typed: {value}\nposted: password={}\nscript: {}\nhtml: {}\nbasic: {}",
                percent(value, true),
                Value::from(value.as_str()),
                escape_html(value, false),
                STANDARD.encode(format!("gui:{value}")),
            );
        }
        text
    }

    fn html(&self) -> String {
        let mut html = format!(
            "<html><head><title>{}</title></head><body><p>{}</p>",
            escape_html(&self.title(), false),
            escape_html(&self.text(), false)
        );
        if let Some(value) = &self.echoed {
            let _ = write!(
                html,
                "<input name=\"echo\" value=\"{}\">",
                escape_html(value, true)
            );
        }
        html.push_str("</body></html>");
        html
    }

    fn elements(&self) -> Vec<Value> {
        let mut elements: Vec<Value> = self
            .page
            .elements
            .iter()
            .enumerate()
            .map(|(index, element)| {
                let id = self.element_id(index);
                let value = if element.password() {
                    "*****".to_owned()
                } else {
                    self.values.get(&id).cloned().unwrap_or_default()
                };
                let mut reported = json!({
                    "backendNodeId": id,
                    "tagName": element.tag,
                    "role": element.role,
                    "name": element.name,
                    "type": "native",
                    "value": value,
                });
                if let Some(kind) = element.kind {
                    reported["inputType"] = kind.to_ascii_lowercase().into();
                }
                reported
            })
            .collect();
        if let Some(value) = &self.echoed {
            elements.push(json!({
                "backendNodeId": self.echo_id(),
                "tagName": "div",
                "role": "status",
                "name": echo_line(value),
                "type": "native",
            }));
        }
        elements
    }

    fn forms(&self) -> Vec<Value> {
        self.page
            .forms
            .iter()
            .enumerate()
            .map(|(index, form)| {
                let fields: Vec<Value> = form
                    .fields
                    .iter()
                    .filter_map(|&field| {
                        let element = self.page.elements.get(field)?;
                        let mut reported = json!({
                            "backendNodeId": self.element_id(field),
                            "tagName": element.tag,
                            "name": element.name,
                            "value": if element.password() { "*****" } else { "" },
                        });
                        if let Some(kind) = element.kind {
                            reported["inputType"] = kind.to_ascii_lowercase().into();
                        }
                        Some(reported)
                    })
                    .collect();
                let mut reported = json!({
                    "backendNodeId": self.form_id(index),
                    "method": "post",
                    "fields": fields,
                });
                if let Some(action) = &form.action {
                    reported["action"] = action.as_str().into();
                }
                reported
            })
            .collect()
    }

    fn describe(&self, id: u64) -> Option<Value> {
        if id == self.base {
            return Some(
                json!({"nodeId": id, "backendNodeId": id, "nodeName": "#document", "localName": ""}),
            );
        }
        if self.echoed.is_some() && id == self.echo_id() {
            return Some(
                json!({"nodeId": id, "backendNodeId": id, "nodeName": "DIV", "localName": "div"}),
            );
        }
        if let Some(form) = self.form(id) {
            let attributes = form
                .action
                .iter()
                .flat_map(|action| ["action", action])
                .collect::<Vec<_>>();
            return Some(
                json!({"nodeId": id, "backendNodeId": id, "nodeName": "FORM", "localName": "form", "attributes": attributes}),
            );
        }
        let (_, element) = self.element(id)?;
        let mut attributes = vec!["name".to_owned(), element.name.clone()];
        if let Some(kind) = element.kind {
            attributes.extend(["type".to_owned(), kind.to_owned()]);
        }
        if let Some(action) = &element.formaction {
            attributes.extend(["formaction".to_owned(), action.clone()]);
        }
        Some(json!({
            "nodeId": id,
            "backendNodeId": id,
            "nodeName": element.tag.to_ascii_uppercase(),
            "localName": element.tag,
            "attributes": attributes,
        }))
    }

    fn select(&self, id: u64, selector: &str) -> Vec<u64> {
        if id == self.base && selector == "input[type=password i]" {
            return (0..self.page.elements.len())
                .filter(|&index| {
                    let element = &self.page.elements[index];
                    element.password() && !element.subframe
                })
                .map(|index| self.element_id(index))
                .collect();
        }
        match self.form(id) {
            Some(form) if selector == "button, input[type=submit], input[type=image]" => form
                .submit
                .map(|index| self.element_id(index))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn click(&mut self, id: u64, shared: &Shared, out: &mut Outbox) {
        let Some((index, element)) = self.element(id) else {
            return;
        };
        let goes = element.goes.clone();
        let submits = self
            .page
            .forms
            .iter()
            .find(|form| form.submit == Some(index))
            .cloned();
        let target = match (&submits, goes) {
            (Some(form), _) => {
                let action = element
                    .formaction
                    .clone()
                    .or_else(|| form.action.clone())
                    .unwrap_or_default();
                Url::parse(&self.page.url)
                    .and_then(|page| page.join(&action))
                    .map(String::from)
                    .ok()
            }
            (None, goes) => goes,
        };
        let Some(target) = target else {
            return;
        };
        let posted = submits.and_then(|form| {
            form.fields
                .iter()
                .find(|&&field| self.page.elements.get(field).is_some_and(Element::password))
                .and_then(|&field| self.values.get(&self.element_id(field)).cloned())
        });
        out.navigation();
        self.navigate(&target, shared, posted);
    }

    fn answer(
        &mut self,
        method: &str,
        params: &Value,
        shared: &mut Shared,
        out: &mut Outbox,
    ) -> Result<Value, String> {
        let node = params["backendNodeId"]
            .as_u64()
            .or_else(|| params["nodeId"].as_u64())
            .unwrap_or_default();
        Ok(match method {
            "Target.createTarget" => json!({"targetId": "T1"}),
            "Target.attachToTarget" => json!({"sessionId": "S1"}),
            "Target.closeTarget" => json!({"success": true}),
            "Target.getTargetInfo" => {
                json!({"targetInfo": {"targetId": "T1", "type": "page", "url": self.url(), "title": self.title()}})
            }
            "Page.enable" | "Input.dispatchKeyEvent" | "Input.dispatchMouseEvent" => json!({}),
            "Page.navigate" => {
                let url = params["url"].as_str().unwrap_or_default().to_owned();
                let known = shared.pages.iter().any(|page| page.url == url);
                out.navigation();
                self.navigate(&url, shared, None);
                if known {
                    json!({"frameId": MAIN_FRAME, "loaderId": format!("L{}", self.base)})
                } else {
                    json!({"frameId": MAIN_FRAME, "loaderId": format!("L{}", self.base), "errorText": "CouldntResolveHost"})
                }
            }
            "Page.getFrameTree" => {
                let mut tree = json!({"frame": {"id": MAIN_FRAME, "url": self.url()}});
                if self.page.elements.iter().any(|element| element.subframe) {
                    tree["childFrames"] = json!([{"frame": {"id": "F2", "parentId": MAIN_FRAME, "url": "about:srcdoc"}}]);
                }
                json!({"frameTree": tree})
            }
            "Page.getNavigationHistory" => json!({
                "currentIndex": self.at,
                "entries": self.history.iter().enumerate().map(|(index, url)| json!({"id": index + 1, "url": url, "title": ""})).collect::<Vec<_>>(),
            }),
            "Page.navigateToHistoryEntry" => {
                let at = params["entryId"]
                    .as_u64()
                    .and_then(|id| usize::try_from(id.checked_sub(1)?).ok())
                    .filter(|at| *at < self.history.len())
                    .ok_or("InvalidParams")?;
                self.at = at;
                let url = self.history[at].clone();
                out.navigation();
                self.load(&url, shared, None);
                json!({"frameId": MAIN_FRAME, "loaderId": format!("L{}", self.base)})
            }
            "LP.getPageStatus" => {
                let mut status = json!({"url": self.url()});
                if self.page.status > 0 {
                    status["httpStatus"] = self.page.status.into();
                }
                status
            }
            "LP.getMarkdown" => {
                let text = match (self.element(node), &self.echoed) {
                    (Some((_, element)), _) => element.name.clone(),
                    (None, Some(value)) if node == self.echo_id() => echo_line(value),
                    (None, _) => self.text(),
                };
                json!({"markdown": markdown(&text)})
            }
            "LP.getInteractiveElements" => json!({"elements": self.elements()}),
            "LP.detectForms" => json!({"forms": self.forms()}),
            "LP.getNodeDetails" => {
                let (_, element) = self.element(node).ok_or("UnknownNode")?;
                json!({"nodeDetails": {"backendNodeId": node, "tagName": element.tag, "role": element.role, "name": element.name}})
            }
            "LP.fillNode" => {
                let (index, element) = self.element(node).ok_or("UnknownNode")?;
                let text = params["text"].as_str().unwrap_or_default().to_owned();
                shared.fills.push(Fill {
                    url: self.url(),
                    element: index,
                    text: text.clone(),
                });
                if element.password() && self.page.echo {
                    self.echoed = Some(text.clone());
                }
                self.values.insert(node, text);
                json!([])
            }
            "LP.clickNode" => {
                self.click(node, shared, out);
                json!([])
            }
            "LP.scrollNode" => json!({"backendNodeId": node, "x": 0, "y": 0}),
            "DOM.getDocument" => {
                json!({"root": {"nodeId": self.base, "backendNodeId": self.base, "nodeName": "#document"}})
            }
            "DOM.getOuterHTML" => json!({"outerHTML": self.html()}),
            "DOM.describeNode" => json!({"node": self.describe(node).ok_or("UnknownNode")?}),
            "DOM.querySelectorAll" => {
                json!({"nodeIds": self.select(node, params["selector"].as_str().unwrap_or_default())})
            }
            "DOM.focus" => {
                self.focused = Some(node);
                json!({})
            }
            "Input.insertText" => {
                if let Some(focused) = self.focused {
                    self.values.insert(
                        focused,
                        params["text"].as_str().unwrap_or_default().to_owned(),
                    );
                }
                json!({})
            }
            method if method.starts_with("Runtime.") => {
                return Err(format!("this fake offers no {method}"));
            }
            method => return Err(format!("'{method}' wasn't found")),
        })
    }
}

fn echo_line(value: &str) -> String {
    format!("You typed {value}")
}

/// Page text as `LP.getMarkdown` writes it: a backslash before ASCII
/// punctuation.
fn markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn percent(value: &str, form: bool) -> String {
    let mut encoded = String::new();
    for &byte in value.as_bytes() {
        let kept = byte.is_ascii_alphanumeric()
            || if form {
                b"*-._".contains(&byte)
            } else {
                b"-_.!~*'()".contains(&byte)
            };
        if kept {
            encoded.push(char::from(byte));
        } else if form && byte == b' ' {
            encoded.push('+');
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn escape_html(value: &str, attribute: bool) -> String {
    let mut escaped = String::new();
    for c in value.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' if !attribute => escaped.push_str("&lt;"),
            '>' if !attribute => escaped.push_str("&gt;"),
            '"' if attribute => escaped.push_str("&quot;"),
            c => escaped.push(c),
        }
    }
    escaped
}
