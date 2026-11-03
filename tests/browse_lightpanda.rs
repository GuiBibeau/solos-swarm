use axum::Router;
use axum::extract::State;
use axum::response::Html;
use axum::routing::{get, post};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use solos_agent_core::model::{AssignmentId, TicketId};
use solos_agent_core::trail::{
    ActText, CloseReason, Digest, PageAction, StepOutcome, TrailAction, TrailKey,
};
use solos_swarm::browse::{
    BlobKind, BrowseCall, Browsed, BrowserConfig, Browsers, Launcher, TrailSigner, verify_step,
};
use solos_swarm::vault::{
    Approval, ItemKind, NewItem, SandboxView, SecretValue, UseContext, Vault, VaultConfig,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PASSWORD: &str = "S3cret p&ss<\"word'>/+\u{e9}";
const POSTED: &str = "S3cret+p%26ss%3C%22word%27%3E%2F%2B%C3%A9";
const FORMS: [&str; 6] = [
    PASSWORD,
    POSTED,
    "S3cret%20p%26ss%3C%22word'%3E%2F%2B%C3%A9",
    "S3cret p&ss<\\\"word'>/+\u{e9}",
    "S3cret p&amp;ss&lt;\"word'&gt;/+\u{e9}",
    "UzNjcmV0IHAmc3M8IndvcmQnPi8rw6k",
];

const LOGIN: &str = r#"<!doctype html><html><head><title>Probe login</title></head><body>
<h1>Sign in</h1>
<form id="f" action="/session" method="post">
<label for="u">Username</label><input id="u" name="username" type="text">
<label for="p">Password</label><input id="p" name="password" type="password">
<select id="s" name="pick"><option>alpha</option><option>beta</option></select>
<button type="submit">Sign in</button>
</form>
<a href="/next">Next page</a>
<div id="echo"></div>
<script>document.getElementById('p').addEventListener('input', e => { document.getElementById('echo').textContent = 'You typed: ' + e.target.value; });</script>
</body></html>"#;

const NEXT: &str =
    "<!doctype html><html><head><title>Next</title></head><body><p>next page</p></body></html>";

type Posted = Arc<Mutex<Vec<String>>>;

async fn session(State(posted): State<Posted>, body: String) -> Html<String> {
    posted.lock().unwrap().push(body.clone());
    Html(format!(
        "<!doctype html><html><head><title>Signed in</title></head><body><p>posted: {}</p></body></html>",
        body.replace('&', "&amp;")
    ))
}

struct Rig {
    browsers: Browsers,
    key: TrailKey,
    n: u32,
    shown: Vec<Browsed>,
}

impl Rig {
    async fn step(&mut self, action: TrailAction, typed: Option<&str>) -> Browsed {
        self.n += 1;
        let call = BrowseCall {
            ticket: TicketId::new("tk_0123456789abcdef"),
            assignment: AssignmentId::new("as_1"),
            n: self.n,
            action,
            typed: typed.map(str::to_owned),
            context: UseContext {
                requester: "slack:U1".into(),
                environment: "local".into(),
                grants: BTreeMap::new(),
                takes_approvals: true,
            },
        };
        let browsed = self.browsers.act(call.clone()).await;
        verify_step(&call, &browsed, &self.key).unwrap();
        let body = &browsed.step.body;
        let outcome = match &body.outcome {
            StepOutcome::Page(page) => format!(
                "page {} {:?} status {:?} filled {:?}",
                page.url, page.title, page.status, page.filled
            ),
            other => format!("{other:?}"),
        };
        println!(
            "n{} session {} step {}: {:?} -> {outcome}",
            body.n,
            body.session,
            body.session_step,
            serde_json::to_value(&body.action).unwrap()["kind"]
        );
        self.shown.push(browsed.clone());
        browsed
    }
}

fn view(browsed: &Browsed) -> Value {
    let view = browsed
        .blobs
        .iter()
        .find(|blob| blob.kind == BlobKind::View)
        .unwrap();
    serde_json::from_slice(&view.bytes).unwrap()
}

fn id(view: &Value, name: &str) -> u32 {
    let elements = view["elements"].as_array().unwrap();
    let element = elements
        .iter()
        .find(|element| element["name"] == name)
        .unwrap_or_else(|| panic!("no element {name:?} in {elements:?}"));
    u32::try_from(element["id"].as_u64().unwrap()).unwrap()
}

fn named(action: PageAction, element: Option<u32>, text: &str) -> TrailAction {
    TrailAction::Act {
        action,
        element,
        text: Some(ActText::Named { text: text.into() }),
    }
}

fn open(url: &str) -> TrailAction {
    TrailAction::Open {
        url: url.into(),
        key: None,
    }
}

fn running_in(dir: &Path) -> Vec<(u32, Vec<String>)> {
    let needle = dir.to_string_lossy().into_owned();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|pid| pid.parse().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        if args.iter().any(|arg| arg.contains(&needle)) {
            found.push((pid, args));
        }
    }
    found
}

fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn descendants(pid: u32) -> Vec<u32> {
    let children =
        std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap_or_default();
    children
        .split_whitespace()
        .filter_map(|child| child.parse().ok())
        .flat_map(|child| std::iter::once(child).chain(descendants(child)))
        .collect()
}

async fn gone(pids: &[u32]) -> bool {
    for _ in 0..100 {
        if pids
            .iter()
            .all(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn start_browsers(
    root: &Path,
    binary: &Path,
    vault: Option<Arc<Vault>>,
    private: bool,
) -> (Browsers, TrailKey) {
    let view = SandboxView::new([]);
    let key_file = root.join(format!("trail-{private}.key"));
    let key = TrailSigner::init(&key_file, &view).unwrap();
    let config: BrowserConfig = serde_json::from_value(json!({
        "lightpanda": binary,
        "audit_key": key_file,
        "allow_private_networks": private,
    }))
    .unwrap();
    let browsers = Browsers::new(
        &config,
        "local",
        &root.join(format!("run-{private}")),
        vault,
        TrailSigner::load(&key_file, &view).unwrap(),
        Launcher::Lightpanda,
    )
    .unwrap();
    (browsers, key)
}

#[tokio::test]
#[ignore = "needs SOLOS_LIGHTPANDA, a Lightpanda binary, and bwrap and prlimit on Linux"]
async fn a_real_lightpanda_signs_in_on_a_local_page_and_shows_no_password() {
    let Ok(binary) = std::env::var("SOLOS_LIGHTPANDA") else {
        eprintln!("skipped: SOLOS_LIGHTPANDA names no Lightpanda binary");
        return;
    };
    let binary = PathBuf::from(binary);
    let id_text = uuid::Uuid::new_v4().simple().to_string();
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("lp-{}", &id_text[..8]));
    std::fs::create_dir_all(&root).unwrap();

    let posted = Posted::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let site = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let app = Router::new()
        .route("/login", get(|| async { Html(LOGIN) }))
        .route("/session", post(session))
        .route("/next", get(|| async { Html(NEXT) }))
        .with_state(Arc::clone(&posted));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let view_paths = SandboxView::new([]);
    let vault_config = VaultConfig {
        path: root.join("vault.enc"),
        key_file: root.join("vault.key"),
    };
    Vault::init(&vault_config, &view_paths).unwrap();
    let vault = Vault::open(&vault_config, &view_paths).unwrap();
    std::fs::write(root.join("password"), PASSWORD).unwrap();
    vault
        .add(
            NewItem {
                name: "github-gui".parse().unwrap(),
                kind: ItemKind::Login {
                    sites: vec![site.parse().unwrap()],
                    username: "gui".into(),
                    hint: String::new(),
                },
                users: BTreeSet::new(),
                approval: Approval::None,
                value: SecretValue::read_once(&root.join("password")).unwrap(),
            },
            false,
        )
        .unwrap();

    let (browsers, key) = start_browsers(&root, &binary, Some(Arc::new(vault)), true);
    println!("trail key {key}");
    let mut rig = Rig {
        browsers,
        key,
        n: 0,
        shown: Vec::new(),
    };
    let login_url = format!("{site}/login");

    let opened = rig.step(open(&login_url), None).await;
    let first = view(&opened);
    assert_eq!(first["status"], 200);
    assert_eq!(first["title"], "Probe login");
    assert_eq!(first["logins"], json!(["github-gui"]));
    let password = id(&first, "Password");
    assert_eq!(first["elements"][password as usize - 1]["secret"], true);
    let provenance = opened.step.body.provenance.clone().unwrap();
    assert_eq!(
        provenance.binary_sha256,
        Digest::of(&std::fs::read(&binary).unwrap()),
        "the first step names the binary that ran"
    );
    let run = root.join("run-true");
    let sandboxes = running_in(&run);
    let outer: Vec<_> = sandboxes
        .iter()
        .filter(|(pid, _)| {
            !sandboxes
                .iter()
                .any(|(other, _)| parent(*pid) == Some(*other))
        })
        .collect();
    assert_eq!(
        outer.len(),
        1,
        "one sandbox, and bwrap's init inside it: {sandboxes:?}"
    );
    let (sandbox, argv) = outer[0];
    println!("bwrap argv: {}", argv.join(" "));
    let mut hasher = Sha256::new();
    for arg in [
        "/usr/bin/prlimit",
        "--as=68719476736",
        "--core=0",
        "--nofile=1024",
        "--",
    ]
    .into_iter()
    .chain(argv.iter().map(String::as_str))
    {
        hasher.update(arg.as_bytes());
        hasher.update([0]);
    }
    assert_eq!(
        provenance.argv_sha256.as_str(),
        format!("sha256:{:x}", hasher.finalize()),
        "the first step names the command line that ran"
    );
    let inside = descendants(*sandbox);
    for pid in &inside {
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        println!(
            "in the sandbox, pid {pid}: {}",
            String::from_utf8_lossy(&cmdline).replace('\0', " ")
        );
    }

    let username = id(&first, "Username");
    let typed = "someone";
    let typing = TrailAction::Act {
        action: PageAction::Type,
        element: Some(username),
        text: Some(ActText::Typed {
            digest: Digest::of(typed.as_bytes()),
            chars: 7,
        }),
    };
    let after_typing = view(&rig.step(typing, Some(typed)).await);
    assert_eq!(
        after_typing["elements"][username as usize - 1]["value"],
        typed
    );
    let combobox = after_typing["elements"]
        .as_array()
        .unwrap()
        .iter()
        .position(|element| element["role"] == "combobox")
        .unwrap();
    rig.step(
        named(PageAction::Select, Some(combobox as u32 + 1), "beta"),
        None,
    )
    .await;
    rig.step(named(PageAction::Press, Some(username), "Tab"), None)
        .await;
    rig.step(named(PageAction::Scroll, None, "down"), None)
        .await;

    let filled = rig
        .step(
            TrailAction::Login {
                login: "github-gui".into(),
                username_element: None,
                password_element: None,
                submit: false,
            },
            None,
        )
        .await;
    let filled = view(&filled);
    assert_eq!(filled["filled"], json!(["username", "password"]));
    assert_eq!(filled["elements"][password as usize - 1]["filled"], true);
    assert!(
        filled["text"]
            .as_str()
            .unwrap()
            .contains("You typed: [vault:github-gui]"),
        "the page's echo is scrubbed: {}",
        filled["text"]
    );

    let signed_in = rig
        .step(
            TrailAction::Login {
                login: "github-gui".into(),
                username_element: None,
                password_element: None,
                submit: true,
            },
            None,
        )
        .await;
    let landed = view(&signed_in);
    assert_eq!(landed["title"], "Signed in");
    assert_eq!(landed["url"], format!("{site}/session"));
    assert_eq!(landed["submitted"], true);
    assert!(
        landed["text"]
            .as_str()
            .unwrap()
            .contains("password=[vault:github-gui]"),
        "{}",
        landed["text"]
    );
    let bodies = posted.lock().unwrap().clone();
    println!(
        "the site received: {}",
        bodies
            .join(" | ")
            .replace(POSTED, "<the password, form-encoded>")
    );
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains(&format!("password={POSTED}")),
        "the fill reached the site"
    );
    assert!(bodies[0].contains("username=gui"));
    assert!(
        bodies[0].contains("pick=beta"),
        "the select step chose beta"
    );

    rig.step(open(&login_url), None).await;
    let next = view(
        &rig.step(
            TrailAction::Act {
                action: PageAction::Click,
                element: Some(id(&first, "Next page")),
                text: None,
            },
            None,
        )
        .await,
    );
    assert_eq!(next["url"], format!("{site}/next"));
    assert_eq!(next["text"].as_str().unwrap().trim(), "next page");
    let back = view(
        &rig.step(
            TrailAction::Act {
                action: PageAction::Back,
                element: None,
                text: None,
            },
            None,
        )
        .await,
    );
    assert_eq!(back["url"], login_url);
    let part = view(
        &rig.step(
            TrailAction::Read {
                part: Some(1),
                element: None,
            },
            None,
        )
        .await,
    );
    assert_eq!(part["text_parts"], 1);

    for (pid, _) in running_in(&run) {
        std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap();
    }
    assert!(
        gone(&inside).await,
        "killing the sandbox ends the browser in it"
    );
    let crashed = rig
        .step(
            TrailAction::Read {
                part: Some(1),
                element: None,
            },
            None,
        )
        .await;
    assert!(
        matches!(
            crashed.step.body.outcome,
            StepOutcome::Closed {
                reason: CloseReason::Crashed,
                ..
            }
        ),
        "{:?}",
        crashed.step.body.outcome
    );

    let reopened = rig.step(open(&login_url), None).await;
    assert_ne!(reopened.step.body.session, opened.step.body.session);
    assert!(reopened.step.body.provenance.is_some());
    let sandboxes = running_in(&run);
    let inside: Vec<u32> = sandboxes
        .iter()
        .flat_map(|(pid, _)| std::iter::once(*pid).chain(descendants(*pid)))
        .collect();
    let closed = rig
        .step(
            TrailAction::Close {
                reason: CloseReason::AssignmentEnded,
            },
            None,
        )
        .await;
    assert!(matches!(
        closed.step.body.outcome,
        StepOutcome::Closed {
            reason: CloseReason::AssignmentEnded,
            ..
        }
    ));
    assert!(gone(&inside).await, "a close leaves no browser running");
    assert!(running_in(&run).is_empty());
    let left: Vec<_> = std::fs::read_dir(&run)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        left.is_empty(),
        "a close removes its session directory: {left:?}"
    );

    for step in &rig.shown {
        let mut seen = vec![serde_json::to_string(&step.step).unwrap()];
        seen.extend(
            step.blobs
                .iter()
                .map(|blob| String::from_utf8(blob.bytes.clone()).unwrap()),
        );
        for text in seen {
            for form in FORMS {
                assert!(
                    !text.contains(form),
                    "step {} shows {form:?}",
                    step.step.body.n
                );
            }
        }
    }
    println!(
        "{} steps, every one signed and verified, no form of the password in any of them",
        rig.shown.len()
    );

    let (blocked, blocked_key) = start_browsers(&root, &binary, None, false);
    let mut rig = Rig {
        browsers: blocked,
        key: blocked_key,
        n: 0,
        shown: Vec::new(),
    };
    let refused = view(&rig.step(open(&login_url), None).await);
    println!(
        "with private networks blocked: {}",
        json!({"status": refused["status"], "error": refused["error"], "title": refused["title"]})
    );
    assert_eq!(refused["status"], Value::Null);
    assert_eq!(refused["error"], "CouldntConnect");
    rig.browsers.shutdown().await;
    assert!(running_in(&root.join("run-false")).is_empty());
    std::fs::remove_dir_all(&root).unwrap();
}
