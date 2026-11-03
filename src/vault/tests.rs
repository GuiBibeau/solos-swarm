use super::*;
use reqwest::Url;
use std::fs;
use std::marker::PhantomData;

struct Probe<T: ?Sized>(PhantomData<T>);

trait Lacks {
    const SERIALIZE: bool = false;
    const DISPLAY: bool = false;
}

impl<T: ?Sized> Lacks for T {}

impl<T: ?Sized + Serialize> Probe<T> {
    const SERIALIZE: bool = true;
}

impl<T: ?Sized + Display> Probe<T> {
    const DISPLAY: bool = true;
}

fn config(dir: &Path) -> VaultConfig {
    VaultConfig {
        path: dir.join("vault.enc"),
        key_file: dir.join("keys/vault.key"),
    }
}

fn scratch() -> (PathBuf, Vault) {
    let dir = std::env::temp_dir().join(format!("vault-test-{}", uuid::Uuid::new_v4()));
    let view = SandboxView::new([]);
    Vault::init(&config(&dir), &view).unwrap();
    (dir.clone(), Vault::open(&config(&dir), &view).unwrap())
}

fn value(text: &str) -> SecretValue {
    SecretValue::parse(Zeroizing::new(text.to_owned())).unwrap()
}

fn name(text: &str) -> SecretName {
    text.parse().unwrap()
}

fn login(item: &str, hint: &str, password: &str, approval: Approval) -> NewItem {
    NewItem {
        name: name(item),
        kind: ItemKind::Login {
            sites: vec!["https://github.com".parse().unwrap()],
            username: "gui".into(),
            hint: hint.into(),
        },
        users: BTreeSet::new(),
        approval,
        value: value(password),
    }
}

fn key(item: &str, secret: &str, approval: Approval) -> NewItem {
    NewItem {
        name: name(item),
        kind: ItemKind::Key {
            hosts: vec!["api.openai.com".parse().unwrap()],
            header: "Authorization".parse().unwrap(),
            scheme: KeyScheme::Bearer,
            about: "research".into(),
        },
        users: BTreeSet::from(["slack:U1".to_owned()]),
        approval,
        value: value(secret),
    }
}

fn context(takes_approvals: bool) -> UseContext {
    UseContext {
        requester: "slack:U1".into(),
        environment: "local".into(),
        grants: BTreeMap::new(),
        takes_approvals,
    }
}

#[test]
fn a_value_has_no_way_to_be_printed_or_serialized() {
    const {
        assert!(<Probe<String>>::SERIALIZE && <Probe<String>>::DISPLAY);
        assert!(!<Probe<SecretValue>>::SERIALIZE && !<Probe<SecretValue>>::DISPLAY);
        assert!(!<Probe<LoginFill>>::SERIALIZE && !<Probe<LoginFill>>::DISPLAY);
        assert!(!<Probe<KeyHeader>>::SERIALIZE && !<Probe<KeyHeader>>::DISPLAY);
        assert!(!<Probe<NewItem>>::SERIALIZE && !<Probe<NewItem>>::DISPLAY);
    }
    assert_eq!(format!("{:?}", value("pw-Probe-7f3a9c")), "SecretValue(..)");
}

#[test]
fn a_value_is_one_line_long_enough_to_scrub() {
    for (text, kept) in [
        ("pw-Probe-7f3a9c\n", Some("pw-Probe-7f3a9c")),
        ("pw-Probe-7f3a9c\r\n", Some("pw-Probe-7f3a9c")),
        ("pw-Probe-7f3a9c", Some("pw-Probe-7f3a9c")),
        ("short\n", None),
        ("pw-Probe\n7f3a9c", None),
        ("pw-Probe-7f3a9c\n\n", None),
        ("pw-Probe\0-7f3a9c", None),
    ] {
        let parsed = SecretValue::parse(Zeroizing::new(text.to_owned()));
        assert_eq!(
            parsed.as_ref().ok().map(SecretValue::reveal),
            kept,
            "{text:?}"
        );
    }
    assert!(SecretValue::parse(Zeroizing::new("x".repeat(MAX_VALUE_BYTES + 1))).is_err());
}

#[test]
fn a_value_comes_out_only_through_an_admission() {
    let (dir, vault) = scratch();
    vault
        .add(
            login("github-gui", "work", "pw-Probe-7f3a9c", Approval::None),
            false,
        )
        .unwrap();
    vault
        .add(
            key("openai", "sk-Pr0be_k3y.value~42", Approval::FirstUse),
            false,
        )
        .unwrap();
    let names: Vec<String> = vault
        .list()
        .unwrap()
        .into_iter()
        .map(|item| item.name.to_string())
        .collect();
    assert_eq!(names, ["github-gui", "openai"]);

    let github = "https://github.com".parse().unwrap();
    let Admission::Allowed(fill) = vault
        .admit_login(&name("github-gui"), &github, &context(false))
        .unwrap()
    else {
        panic!("an unattended login on its own site is refused");
    };
    assert_eq!(fill.reveal_password(), "pw-Probe-7f3a9c");
    assert_eq!(fill.username, "gui");
    assert!(!format!("{fill:?}").contains("Probe"));
    let gist = "https://gist.github.com".parse().unwrap();
    for (item, page) in [
        ("github-gui", &gist),
        ("openai", &github),
        ("missing", &github),
    ] {
        assert!(
            matches!(
                vault
                    .admit_login(&name(item), page, &context(true))
                    .unwrap(),
                Admission::Refused(_)
            ),
            "{item} on {page}"
        );
    }

    let first = Url::parse("https://api.openai.com/v1/models").unwrap();
    match vault
        .admit_key(&name("openai"), &first, &context(true))
        .unwrap()
    {
        Admission::NeedsApproval {
            secret,
            label,
            answerable_by,
        } => {
            assert_eq!(secret, "local/openai");
            assert_eq!(label, "send the API key openai to api.openai.com");
            assert_eq!(answerable_by, ["slack:U1"]);
        }
        other => panic!("a first use did not ask: {other:?}"),
    }
    let mut granted = context(true);
    granted.grants.insert(name("openai"), GrantState::Granted);
    let elsewhere = Url::parse("https://evil.example/v1/models").unwrap();
    assert!(matches!(
        vault
            .admit_key(&name("openai"), &elsewhere, &granted)
            .unwrap(),
        Admission::Refused(_)
    ));
    let Admission::Allowed(header) = vault.admit_key(&name("openai"), &first, &granted).unwrap()
    else {
        panic!("a granted key is refused");
    };
    assert!(!format!("{header:?}").contains("Pr0be"));
    let client = reqwest::Client::new();
    let sent = |url: &str| {
        let mut request = client.get(url).build().unwrap();
        header.reveal_into(&mut request);
        request.headers().get("authorization").map(|value| {
            assert!(value.is_sensitive());
            value.to_str().unwrap().to_owned()
        })
    };
    assert_eq!(
        sent("https://api.openai.com/v1/models").as_deref(),
        Some("Bearer sk-Pr0be_k3y.value~42")
    );
    assert_eq!(sent("https://evil.example/v1/models"), None);
    assert_eq!(sent("http://api.openai.com/v1/models"), None);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn replace_is_the_only_way_to_change_an_item() {
    let (dir, vault) = scratch();
    vault
        .add(
            login("github-gui", "old", "pw-Probe-7f3a9c", Approval::None),
            false,
        )
        .unwrap();
    let before = vault.list().unwrap().remove(0);
    assert!(
        vault
            .add(
                login("github-gui", "new", "pw-Probe-0000", Approval::None),
                false
            )
            .is_err()
    );
    assert!(
        vault
            .add(
                key("github-gui", "sk-Pr0be_k3y.value~42", Approval::None),
                true
            )
            .is_err()
    );
    vault
        .add(
            login("github-gui", "new", "pw-Probe-0000", Approval::None),
            true,
        )
        .unwrap();
    let after = vault.list().unwrap().remove(0);
    assert_eq!(after.created_ms, before.created_ms);
    assert!(matches!(&after.kind, ItemKind::Login { hint, .. } if hint == "new"));
    let github = "https://github.com".parse().unwrap();
    let Admission::Allowed(fill) = vault
        .admit_login(&name("github-gui"), &github, &context(false))
        .unwrap()
    else {
        panic!("the replaced login is refused");
    };
    assert_eq!(fill.reveal_password(), "pw-Probe-0000");
    assert!(vault.remove(&name("github-gui")).unwrap());
    assert!(!vault.remove(&name("github-gui")).unwrap());
    assert!(vault.list().unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_item_that_cannot_be_used_safely_is_refused() {
    let (dir, vault) = scratch();
    let mut for_nobody = login("a", "", "pw-Probe-7f3a9c", Approval::None);
    for_nobody.users.insert("U024BE7LH".into());
    let mut no_sites = login("b", "", "pw-Probe-7f3a9c", Approval::None);
    no_sites.kind = ItemKind::Login {
        sites: Vec::new(),
        username: "gui".into(),
        hint: String::new(),
    };
    let mut no_username = login("c", "", "pw-Probe-7f3a9c", Approval::None);
    no_username.kind = ItemKind::Login {
        sites: vec!["https://github.com".parse().unwrap()],
        username: String::new(),
        hint: String::new(),
    };
    for (case, item) in [
        ("a requester without a source", for_nobody),
        ("no site", no_sites),
        ("no username", no_username),
        (
            "a key no header can carry",
            key("d", "sk-Pr0be-\u{e9}t\u{e9}-42", Approval::None),
        ),
        (
            "a hint with a newline",
            login("e", "two\nlines", "pw-Probe-7f3a9c", Approval::None),
        ),
    ] {
        assert!(vault.add(item, false).is_err(), "{case}");
    }
    for header in ["host", "Cookie", "transfer-encoding", "not a header"] {
        assert!(HeaderField::from_str(header).is_err(), "{header}");
    }
    assert!(vault.list().unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn page_text_markdown_escaped_by_lightpanda_is_scrubbed() {
    let (dir, vault) = scratch();
    vault
        .add(
            login("github-gui", "", "pw-Probe-7f3a9c", Approval::None),
            false,
        )
        .unwrap();
    vault
        .add(key("pin", "20261103", Approval::None), false)
        .unwrap();
    let scrubber = vault.scrubber().unwrap();
    for echoed in [
        "You typed: pw\\-Probe\\-7f3a9c",
        "You typed: pw\\-Probe-7f3a9c",
        "You typed: pw-Probe-7f3a9c",
    ] {
        assert_eq!(scrubber.clean(echoed), "You typed: [vault:github-gui]");
    }
    let mut page = serde_json::json!({
        "text": "echo pw-Probe-7f3a9c",
        "pw-Probe-7f3a9c": ["Basic Z3VpOnB3LVByb2JlLTdmM2E5Yw==", 3],
        "pin": 20261103,
    });
    scrubber.clean_json(&mut page);
    assert_eq!(page["text"], "echo [vault:github-gui]");
    assert_eq!(page["[vault:github-gui]"][1], 3);
    assert_eq!(page["pin"], "[vault:pin]");
    let basic = page["[vault:github-gui]"][0].as_str().unwrap();
    assert!(
        basic.contains("[vault:github-gui]") && !basic.contains("LVByb2JlLTdmM2E5"),
        "{basic}"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_a_regular_value_file_is_deleted() {
    let dir = std::env::temp_dir().join(format!("vault-value-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("value"), "pw-Probe-7f3a9c\n").unwrap();
    std::os::unix::fs::symlink(dir.join("value"), dir.join("link")).unwrap();
    let linked = SecretValue::read_once(&dir.join("link")).unwrap();
    assert_eq!(linked.reveal(), "pw-Probe-7f3a9c");
    assert!(dir.join("link").exists() && dir.join("value").exists());
    SecretValue::read_once(&dir.join("value")).unwrap();
    assert!(!dir.join("value").exists());
    assert!(SecretValue::read_once(Path::new("/dev/null")).is_err());
    assert!(Path::new("/dev/null").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_vault_that_does_not_parse_says_nothing_of_what_it_holds() {
    let (dir, _) = scratch();
    let key = Key::read(&dir.join("keys/vault.key")).unwrap();
    let sealed = key
        .seal(br#"{"items": "pw-Probe-7f3a9c"}"#, b"solos-swarm/vault/v1")
        .unwrap();
    fs::write(dir.join("vault.enc"), sealed).unwrap();
    let error = Vault::open(&config(&dir), &SandboxView::new([])).unwrap_err();
    assert!(!format!("{error:#}").contains("Probe"), "{error:#}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_path_reached_through_a_symlink_is_judged_where_it_leads() {
    let dir = std::env::temp_dir().join(format!("vault-view-{}", uuid::Uuid::new_v4()));
    let mounted = dir.join("mounted");
    fs::create_dir_all(&mounted).unwrap();
    std::os::unix::fs::symlink(&mounted, dir.join("alias")).unwrap();
    let view = SandboxView::new([mounted.clone()]);
    assert!(view.hides(&dir.join("alias/keys/vault.key")).is_err());
    assert!(view.hides(&mounted.join("vault.key")).is_err());
    assert!(view.hides(&dir.join("elsewhere/vault.key")).is_ok());
    assert!(view.hides(Path::new("/etc/solos/vault.key")).is_err());
    assert!(view.hides(Path::new("/usr/local/solos/vault.key")).is_err());
    fs::remove_dir_all(dir).unwrap();
}
