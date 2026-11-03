use serde_json::{Value, json};
use solos_agent_core::model::{AssignmentId, TicketId};
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, ElementSeen, PageAction, StepOutcome, TrailAction, TrailKey,
};
use solos_swarm::browse::fake::{Element, FakeCdp, Fill, Form, Page};
use solos_swarm::browse::{
    BlobKind, BrowseCall, Browsed, BrowserConfig, Browsers, Launcher, NO_SESSION, TrailSigner,
    verify_step,
};
use solos_swarm::vault::{
    Approval, ItemKind, KeyScheme, NewItem, SandboxView, SecretValue, UseContext, Vault,
    VaultConfig,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PASSWORD: &str = "S3cret p&ss<\"word'>/+\u{e9}";
const SITE: &str = "https://site.example";

const FORMS: [&str; 21] = [
    "S3cret p&ss<\"word'>/+\u{e9}",
    "S3cret+p%26ss%3C%22word%27%3E%2F%2B%C3%A9",
    "S3cret%20p%26ss%3C%22word'%3E%2F%2B%C3%A9",
    "S3cret p&ss<\\\"word'>/+\u{e9}",
    "S3cret p&amp;ss&lt;&quot;word&#39;&gt;/+\u{e9}",
    "S3cret p&amp;ss&lt;&quot;word&#x27;&gt;/+\u{e9}",
    "UzNjcmV0IHAmc3M8IndvcmQnPi8rw6k=",
    "UzNjcmV0IHAmc3M8IndvcmQnPi8rw6k",
    "S3cret p\\&ss\\<\\\"word\\'\\>\\/\\+\u{e9}",
    "S3cret p&ss<\"word'>\\/\\+\u{e9}",
    "Z3VpOlMzY3JldCBwJnNzPCJ3b3JkJz4vK8Op",
    "YWJTM2NyZXQgcCZzczwid29yZCc+LyvDqQ==",
    "eDpTM2NyZXQgcCZzczwid29yZCc-LyvDqSE=",
    "S3cret+p%26ss%3c%22word%27%3e%2f%2b%c3%a9",
    "S3cret%20p&ss%3C%22word%27%3E/+%C3%A9",
    "S3cret p&ss<\\\"word'>/+\\u00e9",
    "S3cret p&ss<\\\"word'>\\/+\\u00e9",
    "S3cret p\\u0026ss\\u003c\\\"word'\\u003e/+\u{e9}",
    "S3cret p&amp;ss&lt;\"word'&gt;/+\u{e9}",
    "S3cret p&amp;ss<&quot;word'>/+\u{e9}",
    "S3cret p&amp;ss&lt;&#34;word&#39;&gt;/+\u{e9}",
];

fn markdown_escaped(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            c.is_ascii_punctuation()
                .then_some('\\')
                .into_iter()
                .chain([c])
        })
        .collect()
}

struct World {
    root: PathBuf,
    fake: FakeCdp,
    browsers: Browsers,
    key: TrailKey,
    n: u32,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// On the build's disk, under a short name: the fake's socket path must fit
/// a Unix socket's 107 bytes.
fn scratch() -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("browse-{}", &id[..8]));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn vault(root: &Path, approval: Approval) -> Vault {
    let view = SandboxView::new([]);
    let config = VaultConfig {
        path: root.join("vault.enc"),
        key_file: root.join("vault.key"),
    };
    Vault::init(&config, &view).unwrap();
    let vault = Vault::open(&config, &view).unwrap();
    std::fs::write(root.join("password"), PASSWORD).unwrap();
    let login = NewItem {
        name: "github-gui".parse().unwrap(),
        kind: ItemKind::Login {
            sites: vec![SITE.parse().unwrap()],
            username: "gui".into(),
            hint: String::new(),
        },
        users: BTreeSet::new(),
        approval,
        value: SecretValue::read_once(&root.join("password")).unwrap(),
    };
    vault.add(login, false).unwrap();
    vault
}

fn config(root: &Path, max_sessions: usize) -> BrowserConfig {
    serde_json::from_value(json!({
        "lightpanda": "/nowhere/lightpanda",
        "audit_key": root.join("trail.key"),
        "max_sessions": max_sessions,
    }))
    .unwrap()
}

fn world_with(pages: Vec<Page>, max_sessions: usize, approval: Approval) -> World {
    let root = scratch();
    let view = SandboxView::new([]);
    let vault = vault(&root, approval);
    let key = TrailSigner::init(&root.join("trail.key"), &view).unwrap();
    let signer = TrailSigner::load(&root.join("trail.key"), &view).unwrap();
    let fake = FakeCdp::start(&root.join("cdp.sock"), pages).unwrap();
    let browsers = Browsers::new(
        &config(&root, max_sessions),
        "local",
        &root.join("run"),
        Some(Arc::new(vault)),
        signer,
        Launcher::Socket(fake.socket().to_owned()),
    )
    .unwrap();
    assert_eq!(browsers.key(), key);
    World {
        root,
        fake,
        browsers,
        key,
        n: 0,
    }
}

fn world(pages: Vec<Page>) -> World {
    world_with(pages, 2, Approval::None)
}

fn context() -> UseContext {
    UseContext {
        requester: "slack:U1".into(),
        environment: "local".into(),
        grants: BTreeMap::new(),
        takes_approvals: true,
    }
}

impl World {
    fn call(&mut self, assignment: &str, action: TrailAction, typed: Option<&str>) -> BrowseCall {
        self.n += 1;
        BrowseCall {
            ticket: TicketId::new("tk_0123456789abcdef"),
            assignment: AssignmentId::new(assignment),
            n: self.n,
            action,
            typed: typed.map(str::to_owned),
            context: context(),
        }
    }

    async fn send(&self, call: &BrowseCall) -> Browsed {
        let browsed = self.browsers.act(call.clone()).await;
        if let Err(why) = verify_step(call, &browsed, &self.key) {
            panic!("step {} does not verify: {why}", call.n);
        }
        browsed
    }

    async fn step(&mut self, assignment: &str, action: TrailAction) -> Browsed {
        let call = self.call(assignment, action, None);
        self.send(&call).await
    }
}

fn open(url: &str) -> TrailAction {
    TrailAction::Open {
        url: url.into(),
        key: None,
    }
}

fn act(action: PageAction, element: Option<u32>, text: Option<ActText>) -> TrailAction {
    TrailAction::Act {
        action,
        element,
        text,
    }
}

fn typing(element: u32, text: &str) -> TrailAction {
    let typed = ActText::Typed {
        digest: Digest::of(text.as_bytes()),
        chars: u32::try_from(text.chars().count()).unwrap(),
    };
    act(PageAction::Type, Some(element), Some(typed))
}

fn login(username: Option<u32>, password: Option<u32>, submit: bool) -> TrailAction {
    TrailAction::Login {
        login: "github-gui".into(),
        username_element: username,
        password_element: password,
        submit,
    }
}

fn read(part: Option<u32>, element: Option<u32>) -> TrailAction {
    TrailAction::Read { part, element }
}

fn close() -> TrailAction {
    TrailAction::Close {
        reason: CloseReason::AssignmentEnded,
    }
}

fn view(browsed: &Browsed) -> Value {
    let view = browsed
        .blobs
        .iter()
        .find(|blob| blob.kind == BlobKind::View)
        .expect("a page step carries its view");
    serde_json::from_slice(&view.bytes).unwrap()
}

fn refusal(browsed: &Browsed) -> &str {
    match &browsed.step.body.outcome {
        StepOutcome::Refused { why } => why,
        other => panic!("not refused: {other:?}"),
    }
}

fn login_page(url: &str, action: &str) -> Page {
    Page {
        url: url.into(),
        title: "Sign in".into(),
        status: 200,
        text: "Sign in to the site".into(),
        elements: vec![
            Element::input("text", "Username"),
            Element::input("password", "Password"),
            Element::button("Sign in"),
            Element::textarea("Comment"),
            Element::link("Next", &format!("{SITE}/next")),
        ],
        forms: vec![Form {
            action: Some(action.into()),
            fields: vec![0, 1, 3],
            submit: Some(2),
        }],
        echo: false,
    }
}

fn page(url: &str, title: &str) -> Page {
    Page {
        url: url.into(),
        title: title.into(),
        status: 200,
        text: format!("{title} page"),
        ..Page::default()
    }
}

#[tokio::test]
async fn each_step_is_signed_and_numbered_within_its_session() {
    let mut world = world(vec![
        login_page(&format!("{SITE}/login"), "/session"),
        page(&format!("{SITE}/next"), "Next"),
    ]);
    let opened = world.step("as_1", open(&format!("{SITE}/login"))).await;
    let clicked = world
        .step("as_1", act(PageAction::Click, Some(5), None))
        .await;
    let read = world.step("as_1", read(Some(1), None)).await;
    let closed = world.step("as_1", close()).await;
    let session = opened.step.body.session.clone();
    assert_ne!(session, NO_SESSION);
    for (at, step) in [&opened, &clicked, &read, &closed].into_iter().enumerate() {
        assert_eq!(step.step.body.session, session);
        assert_eq!(step.step.body.session_step, u32::try_from(at).unwrap());
        assert_eq!(step.step.body.environment, "local");
        assert_eq!(step.step.body.provenance.is_some(), at == 0, "step {at}");
    }
    assert_eq!(view(&opened)["step"], 1);
    assert_eq!(view(&opened)["title"], "Sign in");
    assert_eq!(
        view(&opened)["elements"][1],
        json!({"id": 2, "role": "textbox", "name": "Password", "secret": true}),
        "a password input shows no value"
    );
    let StepOutcome::Page(seen) = &clicked.step.body.outcome else {
        panic!("{:?}", clicked.step.body.outcome);
    };
    assert_eq!(seen.url, format!("{SITE}/next"));
    assert_eq!(seen.status, Some(200));
    assert_eq!(
        seen.target,
        Some(ElementSeen {
            role: "link".into(),
            name: "Next".into(),
        })
    );
    assert_eq!(view(&read)["text"], "Next page");
    assert_eq!(
        closed.step.body.outcome,
        StepOutcome::Closed {
            steps: 4,
            reason: CloseReason::AssignmentEnded,
        }
    );
    let again = world.step("as_1", open(&format!("{SITE}/next"))).await;
    assert_ne!(again.step.body.session, session, "a close ends the session");
    assert_eq!(again.step.body.session_step, 0);
    assert!(again.step.body.provenance.is_some());
}

#[tokio::test]
async fn a_step_that_was_changed_or_signed_by_another_key_does_not_verify() {
    let mut world = world(vec![page(&format!("{SITE}/next"), "Next")]);
    let call = world.call("as_1", open(&format!("{SITE}/next")), None);
    let browsed = world.send(&call).await;
    let mut edited = browsed.clone();
    if let StepOutcome::Page(seen) = &mut edited.step.body.outcome {
        seen.title = "Another".into();
    }
    assert_eq!(
        verify_step(&call, &edited, &world.key).unwrap_err(),
        "the signature does not verify"
    );
    let other = TrailSigner::init(&world.root.join("other.key"), &SandboxView::new([])).unwrap();
    assert!(verify_step(&call, &browsed, &other).is_err());
    let mut asked = call.clone();
    asked.action = open(&format!("{SITE}/elsewhere"));
    assert!(verify_step(&asked, &browsed, &world.key).is_err());
    let mut renumbered = call.clone();
    renumbered.n += 1;
    assert!(verify_step(&renumbered, &browsed, &world.key).is_err());
    let mut flipped = browsed.clone();
    flipped.blobs[1].bytes[0] ^= 1;
    assert!(verify_step(&call, &flipped, &world.key).is_err());
    let mut dropped = browsed.clone();
    dropped.blobs.pop();
    assert!(verify_step(&call, &dropped, &world.key).is_err());
    assert!(verify_step(&call, &browsed, &world.key).is_ok());
}

#[tokio::test]
async fn a_login_fills_the_password_input_of_its_own_site_and_submits_there() {
    let mut world = world(vec![
        login_page(&format!("{SITE}/login"), "/session"),
        page(&format!("{SITE}/session"), "Signed in"),
    ]);
    world.step("as_1", open(&format!("{SITE}/login"))).await;
    let signed_in = world.step("as_1", login(None, None, true)).await;
    let at = format!("{SITE}/login");
    assert_eq!(
        world.fake.fills(),
        [
            Fill {
                url: at.clone(),
                element: 0,
                text: "gui".into(),
            },
            Fill {
                url: at,
                element: 1,
                text: PASSWORD.into(),
            },
        ]
    );
    let StepOutcome::Page(seen) = &signed_in.step.body.outcome else {
        panic!("{:?}", signed_in.step.body.outcome);
    };
    assert_eq!(seen.url, format!("{SITE}/session"));
    assert_eq!(seen.filled, ["username", "password"]);
    let shown = view(&signed_in);
    assert_eq!(shown["title"], "Signed in");
    assert_eq!(shown["filled"], json!(["username", "password"]));
    assert_eq!(shown["submitted"], true);
}

#[tokio::test]
async fn a_login_fills_nothing_unless_every_check_passes() {
    let evil = "https://evil.example";
    let mut framed = login_page(&format!("{SITE}/framed"), "/session");
    framed.elements[1].subframe = true;
    let mut formaction = login_page(&format!("{SITE}/formaction"), "/session");
    formaction.elements[2].formaction = Some(format!("{evil}/steal"));
    let mut world = world(vec![
        login_page(&format!("{evil}/login"), "/session"),
        login_page("http://site.example/login", "/session"),
        login_page("https://www.site.example/login", "/session"),
        login_page(&format!("{SITE}/elsewhere"), &format!("{evil}/steal")),
        login_page(&format!("{SITE}/fields"), "/session"),
        formaction,
        framed,
    ]);
    let not_main = "is not a password input of this page's main document";
    let posts_off_site = "the form posts to https://evil.example, not to github-gui's site";
    let cases = [
        (
            "https://evil.example/login",
            login(None, None, true),
            "https://evil.example is not one of github-gui's sites",
        ),
        (
            "http://site.example/login",
            login(None, None, true),
            "http://site.example/login is not https",
        ),
        (
            "https://www.site.example/login",
            login(None, None, true),
            "https://www.site.example is not one of github-gui's sites",
        ),
        (
            "https://site.example/elsewhere",
            login(None, None, true),
            posts_off_site,
        ),
        (
            "https://site.example/formaction",
            login(None, None, false),
            posts_off_site,
        ),
        (
            "https://site.example/fields",
            login(None, Some(1), true),
            not_main,
        ),
        (
            "https://site.example/fields",
            login(None, Some(4), true),
            not_main,
        ),
        (
            "https://site.example/fields",
            login(Some(4), Some(2), true),
            "element 4 is not a text, email or tel input",
        ),
        (
            "https://site.example/fields",
            login(None, Some(9), true),
            "element 9 is not in the latest view",
        ),
        (
            "https://site.example/framed",
            login(None, Some(2), true),
            not_main,
        ),
        (
            "https://site.example/framed",
            login(None, None, true),
            not_main,
        ),
    ];
    for (url, action, why) in cases {
        world.step("as_1", open(url)).await;
        let refused = world.step("as_1", action).await;
        assert!(
            refusal(&refused).contains(why),
            "{url}: {}",
            refusal(&refused)
        );
    }
    assert!(world.fake.fills().is_empty());
}

#[tokio::test]
async fn a_login_fills_nothing_when_the_page_moves_before_the_fill() {
    let mut world = world(vec![
        login_page(&format!("{SITE}/login"), "/session"),
        login_page("https://evil.example/login", "/session"),
    ]);
    world.step("as_1", open(&format!("{SITE}/login"))).await;
    world
        .fake
        .move_on("Page.getFrameTree", 1, "https://evil.example/login");
    let refused = world.step("as_1", login(None, None, true)).await;
    assert_eq!(
        refusal(&refused),
        "the page moved to https://evil.example/login before the fill"
    );
    assert!(world.fake.fills().is_empty());
}

#[tokio::test]
async fn a_first_use_waits_for_a_person_and_fills_nothing() {
    let mut world = world_with(
        vec![login_page(&format!("{SITE}/login"), "/session")],
        2,
        Approval::FirstUse,
    );
    world.step("as_1", open(&format!("{SITE}/login"))).await;
    let asked = world.step("as_1", login(None, None, true)).await;
    assert_eq!(
        asked.step.body.outcome,
        StepOutcome::NeedsApproval {
            secret: "local/github-gui".into(),
            label: "sign in to https://site.example as gui (login github-gui)".into(),
        }
    );
    assert!(asked.blobs.is_empty());
    assert!(world.fake.fills().is_empty());
}

#[tokio::test]
async fn no_form_of_a_filled_password_reaches_a_view_a_blob_or_a_signed_body() {
    let mut echoing = login_page(&format!("{SITE}/login"), "/session");
    echoing.echo = true;
    let landing = Page {
        echo: true,
        ..page(&format!("{SITE}/session"), "Signed in")
    };
    let mut world = world(vec![echoing, landing]);
    let mut steps = vec![world.step("as_1", open(&format!("{SITE}/login"))).await];
    let filled = world.step("as_1", login(None, None, false)).await;
    let shown = view(&filled);
    assert_eq!(shown["elements"][1]["filled"], true);
    assert_eq!(shown["elements"][5]["name"], "You typed [vault:github-gui]");
    assert!(
        shown["text"]
            .as_str()
            .unwrap()
            .matches("[vault:github-gui]")
            .count()
            >= 5,
        "the page echoed the password in its text: {}",
        shown["text"]
    );
    steps.push(filled);
    steps.push(world.step("as_1", read(None, Some(6))).await);
    steps.push(world.step("as_1", login(None, None, true)).await);
    let landed = view(steps.last().unwrap());
    assert_eq!(
        landed["url"],
        format!("{SITE}/session?echo=[vault:github-gui]")
    );
    assert_eq!(landed["title"], "Signed in [vault:github-gui]");
    steps.push(world.step("as_1", read(Some(1), None)).await);
    assert_eq!(world.fake.fills().len(), 4, "two logins filled twice each");
    let forms: Vec<String> = FORMS
        .iter()
        .flat_map(|form| [(*form).to_owned(), markdown_escaped(form)])
        .collect();
    for (at, step) in steps.iter().enumerate() {
        let body = serde_json::to_vec(&step.step).unwrap();
        let mut seen = vec![("signed body", body)];
        seen.extend(step.blobs.iter().map(|blob| {
            (
                if blob.kind == BlobKind::View {
                    "view"
                } else {
                    "snapshot"
                },
                blob.bytes.clone(),
            )
        }));
        for (what, bytes) in seen {
            let text = String::from_utf8(bytes).unwrap();
            for form in &forms {
                assert!(
                    !text.contains(form.as_str()),
                    "step {at}'s {what} holds {form:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn the_client_sends_no_runtime_method() {
    let mut world = world(vec![
        login_page(&format!("{SITE}/login"), "/session"),
        page(&format!("{SITE}/session"), "Signed in"),
        page(&format!("{SITE}/next"), "Next"),
    ]);
    world.step("as_1", open(&format!("{SITE}/login"))).await;
    let call = world.call("as_1", typing(1, "someone"), Some("someone"));
    let typed = world.send(&call).await;
    assert_eq!(view(&typed)["elements"][0]["value"], "someone");
    world
        .step(
            "as_1",
            act(
                PageAction::Press,
                Some(1),
                Some(ActText::Named { text: "Tab".into() }),
            ),
        )
        .await;
    world
        .step(
            "as_1",
            act(
                PageAction::Scroll,
                None,
                Some(ActText::Named {
                    text: "down".into(),
                }),
            ),
        )
        .await;
    world.step("as_1", login(None, None, true)).await;
    let back = world.step("as_1", act(PageAction::Back, None, None)).await;
    assert_eq!(view(&back)["url"], format!("{SITE}/login"));
    let username = world.step("as_1", read(None, Some(1))).await;
    assert_eq!(view(&username)["text"], "Username");
    assert_eq!(view(&username)["element"], 1);
    world
        .step("as_1", act(PageAction::Click, Some(5), None))
        .await;
    world.step("as_1", close()).await;
    let methods = world.fake.methods();
    assert!(methods.iter().any(|method| method == "LP.fillNode"));
    assert!(
        !methods.iter().any(|method| method.starts_with("Runtime.")),
        "{methods:?}"
    );
    let browse = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/browse");
    for entry in std::fs::read_dir(browse).unwrap() {
        let path = entry.unwrap().path();
        if path.ends_with("fake.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
            !source.contains("\"Runtime."),
            "{} names a Runtime method",
            path.display()
        );
    }
}

#[tokio::test]
async fn typing_is_refused_before_any_browser_call_unless_the_text_matches_its_digest() {
    let mut world = world(vec![login_page(&format!("{SITE}/login"), "/session")]);
    world.step("as_1", open(&format!("{SITE}/login"))).await;
    let sent = world.fake.methods().len();
    for (action, typed) in [
        (typing(1, "what was asked"), Some("something else")),
        (typing(1, "what was asked"), None),
        (act(PageAction::Click, Some(1), None), Some("stowaway")),
    ] {
        let call = world.call("as_1", action, typed);
        let refused = world.send(&call).await;
        refusal(&refused);
    }
    assert_eq!(world.fake.methods().len(), sent, "no browser call was made");
    let call = world.call("as_1", typing(2, "a guess"), Some("a guess"));
    let refused = world.send(&call).await;
    assert_eq!(
        refusal(&refused),
        "element 2 is a password field; browser_login fills passwords"
    );
}

#[tokio::test]
async fn a_machine_runs_at_most_max_sessions_browsers_and_refuses_the_next_signed() {
    let url = format!("{SITE}/next");
    let mut world = world(vec![page(&url, "Next")]);
    world.step("as_1", open(&url)).await;
    world.step("as_2", open(&url)).await;
    let refused = world.step("as_3", open(&url)).await;
    assert_eq!(
        refusal(&refused),
        "this machine already runs its most browsers (2)"
    );
    assert_eq!(refused.step.body.session, NO_SESSION);
    assert_eq!(refused.step.body.provenance, None);
    world.step("as_1", close()).await;
    let opened = world.step("as_3", open(&url)).await;
    assert!(matches!(opened.step.body.outcome, StepOutcome::Page(_)));
}

#[tokio::test]
async fn a_browser_that_stops_closes_its_session_signed_as_crashed() {
    let mut world = world(vec![login_page(&format!("{SITE}/login"), "/session")]);
    let opened = world.step("as_1", open(&format!("{SITE}/login"))).await;
    world.fake.crash_on("LP.clickNode");
    let crashed = world
        .step("as_1", act(PageAction::Click, Some(5), None))
        .await;
    assert_eq!(crashed.step.body.session, opened.step.body.session);
    assert_eq!(
        crashed.step.body.outcome,
        StepOutcome::Closed {
            steps: 2,
            reason: CloseReason::Crashed,
        }
    );
    let again = world.step("as_1", open(&format!("{SITE}/login"))).await;
    assert_ne!(again.step.body.session, opened.step.body.session);
    assert_eq!(again.step.body.session_step, 0);
}

#[tokio::test]
async fn a_close_with_no_browser_and_a_step_out_of_order_are_answered_signed() {
    let url = format!("{SITE}/next");
    let mut world = world(vec![page(&url, "Next")]);
    let closed = world.step("as_9", close()).await;
    assert_eq!(closed.step.body.session, NO_SESSION);
    assert_eq!(
        closed.step.body.outcome,
        StepOutcome::Closed {
            steps: 0,
            reason: CloseReason::AssignmentEnded,
        }
    );
    let call = world.call("as_1", open(&url), None);
    world.send(&call).await;
    let replayed = world.send(&call).await;
    assert_eq!(
        refusal(&replayed),
        format!("step {} is not after step {}", call.n, call.n)
    );
}

#[tokio::test]
async fn a_socket_path_the_kernel_or_lightpanda_would_refuse_is_refused_at_start() {
    let root = scratch();
    let view = SandboxView::new([]);
    TrailSigner::init(&root.join("trail.key"), &view).unwrap();
    for (socket, why) in [
        (
            format!("/{}/cdp.sock", "x".repeat(110)),
            "a Unix socket path is at most 107",
        ),
        ("/tmp/a:b/cdp.sock".to_owned(), "holds ':'"),
    ] {
        let signer = TrailSigner::load(&root.join("trail.key"), &view).unwrap();
        let refused = Browsers::new(
            &config(&root, 2),
            "local",
            &root.join("run"),
            None,
            signer,
            Launcher::Socket(PathBuf::from(socket)),
        )
        .err()
        .unwrap();
        assert!(refused.to_string().contains(why), "{refused}");
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_trail_key_is_never_replaced_or_kept_where_a_sandbox_reads() {
    let root = scratch();
    let view = SandboxView::new([]);
    let key = TrailSigner::init(&root.join("trail.key"), &view).unwrap();
    assert!(key.as_str().starts_with("ed25519:"));
    assert_eq!(
        TrailSigner::load(&root.join("trail.key"), &view)
            .unwrap()
            .public(),
        key
    );
    assert!(TrailSigner::init(&root.join("trail.key"), &view).is_err());
    let watched = SandboxView::new([root.clone()]);
    assert!(TrailSigner::load(&root.join("trail.key"), &watched).is_err());
    assert!(TrailSigner::init(&root.join("other.key"), &watched).is_err());
    assert!(TrailSigner::init(Path::new("/etc/solos-trail.key"), &view).is_err());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn browser_init_key_writes_a_private_seed_and_prints_its_public_half() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch();
    let out = root.join("audit.key");
    let init = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_solos-swarm"))
            .args(["browser", "init-key", "--out"])
            .arg(&out)
            .output()
            .unwrap()
    };
    let first = init();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let key = TrailSigner::load(&out, &SandboxView::new([]))
        .unwrap()
        .public();
    assert_eq!(String::from_utf8(first.stdout).unwrap(), format!("{key}\n"));
    assert_eq!(
        std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::read(&out).unwrap().len(), 32);
    assert!(
        !init().status.success(),
        "an existing key is never replaced"
    );
    assert_eq!(
        TrailSigner::load(&out, &SandboxView::new([]))
            .unwrap()
            .public(),
        key
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn an_item_added_while_a_session_runs_is_scrubbed_from_its_next_step() {
    let late = "late-Value-91c2e7";
    let url = format!("{SITE}/shows");
    let mut world = world(vec![Page {
        text: format!("the late key is {late}"),
        ..page(&url, "Shows")
    }]);
    let before = world.step("as_1", open(&url)).await;
    assert_eq!(view(&before)["text"], format!("the late key is {late}"));
    let config = VaultConfig {
        path: world.root.join("vault.enc"),
        key_file: world.root.join("vault.key"),
    };
    let vault = Vault::open(&config, &SandboxView::new([])).unwrap();
    std::fs::write(world.root.join("late"), late).unwrap();
    let key = NewItem {
        name: "late-key".parse().unwrap(),
        kind: ItemKind::Key {
            hosts: vec!["api.example.com".parse().unwrap()],
            header: "x-api-key".parse().unwrap(),
            scheme: KeyScheme::Raw,
            about: String::new(),
        },
        users: BTreeSet::new(),
        approval: Approval::None,
        value: SecretValue::read_once(&world.root.join("late")).unwrap(),
    };
    vault.add(key, false).unwrap();
    let after = world.step("as_1", read(Some(1), None)).await;
    assert_eq!(after.step.body.session, before.step.body.session);
    assert_eq!(view(&after)["text"], "the late key is [vault:late-key]");
}
