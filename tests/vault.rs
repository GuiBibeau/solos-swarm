use solos_swarm::sealed::TEMPORARY_PREFIX;
use solos_swarm::vault::{
    Approval, ItemKind, KeyScheme, NewItem, SandboxView, SecretValue, Vault, VaultConfig,
};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const PASSWORD: &str = "S3cret p&ss<\"word'>/+\u{e9}";
const KEY: &str = "sk-Pr0be_k3y.value~42";
const PROBE: &str = "pw-Probe-7f3a9c";

const ENCODINGS: [(&str, &[&str]); 3] = [
    (
        PASSWORD,
        &[
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
        ],
    ),
    (
        KEY,
        &[
            "sk-Pr0be_k3y.value~42",
            "sk-Pr0be_k3y.value%7E42",
            "c2stUHIwYmVfazN5LnZhbHVlfjQy",
            "sk\\-Pr0be\\_k3y\\.value\\~42",
        ],
    ),
    (
        PROBE,
        &[
            "pw-Probe-7f3a9c",
            "cHctUHJvYmUtN2YzYTlj",
            "pw\\-Probe\\-7f3a9c",
            "pw-Probe\\-7f3a9c",
        ],
    ),
];

const AGENT_YAML: &str = "
policy:
  people: [slack:alice]
  bots: [github:ci]
sources:
  - kind: jsonl
    path: triggers.jsonl
channels:
  - kind: jsonl
    source: slack
    path: out/outbox.jsonl
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
";

const NODE_YAML: &str = "
name: laptop
agent:
  address: 127.0.0.1:7443
  fingerprint: sha256:0000000000000000000000000000000000000000000000000000000000000000
cert: tls/node.crt
key: tls/node.key
work_root: work
sandbox:
  mounts: [{target: /data, source: ROOT/data}]
";

const AGENT_PI_YAML: &str = "
policy:
  people: [slack:alice]
sources:
  - kind: jsonl
    path: triggers.jsonl
channels:
  - kind: jsonl
    source: slack
    path: out/outbox.jsonl
router: {kind: scripted}
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
  run_code:
    mounts: [{target: /repo, source: ROOT/repo}]
  claude: {binary: ROOT/claude/cli.js, node: ROOT/node/bin/node}
budgets: {session_microusd: 500000}
";

struct Scratch {
    root: PathBuf,
    printed: String,
}

impl Scratch {
    fn new(config: &str, vault: &str) -> Self {
        let root = std::env::temp_dir().join(format!("vault-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let config =
            format!("{config}vault: {vault}\n").replace("ROOT", &root.display().to_string());
        fs::write(root.join("config.yaml"), config).unwrap();
        Self {
            root,
            printed: String::new(),
        }
    }

    fn config(&self) -> String {
        self.root.join("config.yaml").display().to_string()
    }

    fn vault_config(&self) -> VaultConfig {
        VaultConfig {
            path: self.root.join("state/vault.enc"),
            key_file: self.root.join("keys/vault.key"),
        }
    }

    fn vault(&mut self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_solos-swarm"))
            .arg("vault")
            .args(args)
            .args(["--config", &self.config()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        self.printed
            .push_str(&String::from_utf8_lossy(&output.stdout));
        self.printed
            .push_str(&String::from_utf8_lossy(&output.stderr));
        output
    }

    fn ok(&mut self, args: &[&str], stdin: &str) -> String {
        let output = self.vault(args, stdin);
        assert!(
            output.status.success(),
            "vault {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn refused(&mut self, args: &[&str], stdin: &str, why: &str) {
        let output = self.vault(args, stdin);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "vault {args:?} succeeded");
        assert!(
            stderr.contains(why),
            "vault {args:?} said {stderr:?}, not {why:?}"
        );
    }

    fn value_file(&self, name: &str, value: &str) -> String {
        let path = self.root.join(name);
        fs::write(&path, format!("{value}\n")).unwrap();
        path.display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const VAULT: &str = "{path: state/vault.enc, key_file: keys/vault.key}";

#[test]
fn nothing_the_cli_prints_holds_a_value() {
    let mut scratch = Scratch::new(AGENT_YAML, VAULT);
    scratch.ok(&["init"], "");
    scratch.ok(&["init"], "");
    scratch.ok(
        &[
            "add-login",
            "--name",
            "github-gui",
            "--site",
            "https://github.com",
            "--username",
            "gui",
            "--hint",
            "Gui's work account",
            "--for",
            "slack:U024BE7LH",
            "--password-file",
            "-",
        ],
        &format!("{PASSWORD}\n"),
    );
    let key_file = scratch.value_file("openai.secret", KEY);
    scratch.ok(
        &[
            "add-key",
            "--name",
            "openai",
            "--host",
            "api.openai.com",
            "--header",
            "authorization",
            "--scheme",
            "bearer",
            "--about",
            "OpenAI API for research tickets",
            "--approval",
            "none",
            "--value-file",
            &key_file,
        ],
        "",
    );
    assert!(!Path::new(&key_file).exists(), "the value file was kept");
    let probe_file = scratch.value_file("probe.secret", PROBE);
    scratch.ok(
        &[
            "add-login",
            "--name",
            "probe",
            "--site",
            "http://localhost:3000",
            "--username",
            "probe",
            "--password-file",
            &probe_file,
        ],
        "",
    );

    let table = scratch.ok(&["list"], "");
    let lines: Vec<Vec<&str>> = table
        .lines()
        .map(|line| {
            line.split("  ")
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .collect()
        })
        .collect();
    assert_eq!(lines.len(), 3, "{table}");
    assert_eq!(
        lines[0][..7],
        [
            "LOGIN",
            "github-gui",
            "https://github.com",
            "gui",
            "\"Gui's work account\"",
            "first-use",
            "for slack:U024BE7LH"
        ]
    );
    assert_eq!(
        lines[1][..7],
        [
            "KEY",
            "openai",
            "api.openai.com",
            "authorization: Bearer …",
            "\"OpenAI API for research tickets\"",
            "none",
            "any requester"
        ]
    );
    let listed: serde_json::Value =
        serde_json::from_str(&scratch.ok(&["list", "--json"], "")).unwrap();
    assert_eq!(listed[0]["name"], "github-gui");
    assert_eq!(listed[0]["kind"], "login");
    assert_eq!(
        listed[0]["sites"],
        serde_json::json!(["https://github.com"])
    );
    assert_eq!(listed[0]["username"], "gui");
    assert_eq!(listed[0]["hint"], "Gui's work account");
    assert_eq!(listed[0]["users"], serde_json::json!(["slack:U024BE7LH"]));
    assert_eq!(listed[0]["approval"], "first-use");
    assert_eq!(listed[1]["hosts"], serde_json::json!(["api.openai.com"]));
    assert_eq!(listed[1]["header"], "authorization");
    assert_eq!(listed[1]["scheme"], "bearer");
    assert_eq!(listed[1]["approval"], "none");

    let again = [
        "add-login",
        "--name",
        "github-gui",
        "--site",
        "https://github.com",
        "--username",
        "gui",
        "--hint",
        "changed",
    ];
    scratch.refused(&again, PASSWORD, "--replace");
    scratch.refused(
        &[
            "add-key",
            "--name",
            "github-gui",
            "--host",
            "github.com",
            "--header",
            "authorization",
            "--scheme",
            "raw",
            "--replace",
        ],
        KEY,
        "remove it first",
    );
    let short = scratch.value_file("short.secret", "short");
    scratch.refused(
        &[
            "add-key",
            "--name",
            "short",
            "--host",
            "a.example",
            "--header",
            "x-api-key",
            "--scheme",
            "raw",
            "--value-file",
            &short,
        ],
        "",
        "shorter than 8 bytes",
    );
    assert!(!Path::new(&short).exists(), "a refused value file was kept");
    scratch.refused(
        &[
            "add-key",
            "--name",
            "wide",
            "--host",
            "a.example",
            "--header",
            "x-api-key",
            "--scheme",
            "raw",
        ],
        PASSWORD,
        "printable ASCII",
    );
    scratch.refused(
        &["remove", "--name", "missing"],
        "",
        "holds no item missing",
    );

    let vault_file = scratch.root.join("state/vault.enc");
    let sealed = fs::read(&vault_file).unwrap();
    let mut tampered: serde_json::Value = serde_json::from_slice(&sealed).unwrap();
    let byte = tampered["ciphertext"][3].as_u64().unwrap();
    tampered["ciphertext"][3] = (byte ^ 0x20).into();
    fs::write(&vault_file, serde_json::to_vec(&tampered).unwrap()).unwrap();
    scratch.refused(&["list"], "", "does not open under its key");
    fs::write(&vault_file, &sealed).unwrap();

    let key_path = scratch.root.join("keys/vault.key");
    let key = fs::read(&key_path).unwrap();
    fs::write(&key_path, [7u8; 32]).unwrap();
    scratch.refused(&["list", "--json"], "", "does not open under its key");
    fs::write(&key_path, &key).unwrap();
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o640)).unwrap();
    scratch.refused(&["list"], "", "open to group or others");
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o604)).unwrap();
    scratch.refused(&["list"], "", "open to group or others");
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();

    let mut replaced = again.to_vec();
    replaced.push("--replace");
    scratch.ok(&replaced, PASSWORD);
    scratch.ok(&["remove", "--name", "openai"], "");
    let listed: serde_json::Value =
        serde_json::from_str(&scratch.ok(&["list", "--json"], "")).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["hint"], "changed");

    for (value, forms) in ENCODINGS {
        for form in forms {
            assert!(
                !scratch.printed.contains(form),
                "the CLI printed {form:?}, a form of {value:?}:\n{}",
                scratch.printed
            );
        }
    }
}

#[test]
fn a_scrubber_built_after_an_add_scrubs_the_new_value() {
    let mut scratch = Scratch::new(AGENT_YAML, VAULT);
    scratch.ok(&["init"], "");
    let vault = Vault::open(&scratch.vault_config(), &SandboxView::new([])).unwrap();
    let before = vault.scrubber().unwrap();
    scratch.ok(
        &[
            "add-login",
            "--name",
            "github-gui",
            "--site",
            "https://github.com",
            "--username",
            "gui",
        ],
        PASSWORD,
    );
    scratch.ok(
        &[
            "add-key",
            "--name",
            "openai",
            "--host",
            "api.openai.com",
            "--header",
            "authorization",
            "--scheme",
            "bearer",
        ],
        KEY,
    );
    scratch.ok(
        &[
            "add-login",
            "--name",
            "probe",
            "--site",
            "http://localhost:3000",
            "--username",
            "probe",
        ],
        PROBE,
    );
    let after = vault.scrubber().unwrap();
    for ((value, forms), marker) in
        ENCODINGS
            .into_iter()
            .zip(["[vault:github-gui]", "[vault:openai]", "[vault:probe]"])
    {
        for form in forms {
            let page = format!("<p>{form}</p>");
            assert_eq!(before.clean(&page), page);
            let cleaned = after.clean(&page);
            assert!(
                cleaned.contains(marker) && !cleaned.contains(form),
                "{form:?}, a form of {value:?}, became {cleaned:?}"
            );
        }
    }
    assert_eq!(
        after.clean("Lightpanda's markdown: pw\\-Probe\\-7f3a9c."),
        "Lightpanda's markdown: [vault:probe]."
    );
}

#[test]
#[ignore = "a writer process that the tests below start"]
fn writer() {
    let (Ok(dir), Ok(prefix), Ok(count)) = (
        std::env::var("SOLOS_VAULT_DIR"),
        std::env::var("SOLOS_VAULT_PREFIX"),
        std::env::var("SOLOS_VAULT_COUNT"),
    ) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let vault = Vault::open(&writer_config(&dir), &SandboxView::new([])).unwrap();
    let names = match count.as_str() {
        "forever" => vault.list().unwrap().len()..usize::MAX,
        count => 0..count.parse().unwrap(),
    };
    for n in names {
        let value_file = dir.join(format!("{prefix}-{n}.secret"));
        fs::write(&value_file, format!("value of {prefix}-{n:08}")).unwrap();
        let item = NewItem {
            name: format!("{prefix}-{n}").parse().unwrap(),
            kind: ItemKind::Key {
                hosts: vec!["api.example.com".parse().unwrap()],
                header: "x-api-key".parse().unwrap(),
                scheme: KeyScheme::Raw,
                about: String::new(),
            },
            users: BTreeSet::new(),
            approval: Approval::None,
            value: SecretValue::read_once(&value_file).unwrap(),
        };
        vault.add(item, false).unwrap();
    }
}

fn writer_config(dir: &Path) -> VaultConfig {
    VaultConfig {
        path: dir.join("vault.enc"),
        key_file: dir.join("vault.key"),
    }
}

fn start_writer(dir: &Path, prefix: &str, count: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "writer",
            "--exact",
            "--ignored",
            "--quiet",
            "--test-threads=1",
        ])
        .env("SOLOS_VAULT_DIR", dir)
        .env("SOLOS_VAULT_PREFIX", prefix)
        .env("SOLOS_VAULT_COUNT", count)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// On the build's disk rather than in `/tmp`, which can be a tmpfs where a
/// sync costs nothing and a write is over before a kill can land in it.
fn writer_dir() -> (PathBuf, Vault) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vault-writers-{}", uuid::Uuid::new_v4()));
    let view = SandboxView::new([]);
    Vault::init(&writer_config(&dir), &view).unwrap();
    let vault = Vault::open(&writer_config(&dir), &view).unwrap();
    (dir, vault)
}

#[test]
fn two_processes_adding_at_once_keep_every_item() {
    let (dir, vault) = writer_dir();
    let writers = [start_writer(&dir, "a", "25"), start_writer(&dir, "b", "25")];
    let deadline = Instant::now() + Duration::from_secs(120);
    for mut writer in writers {
        let status = loop {
            if let Some(status) = writer.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "a writer never finished");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "a writer failed");
    }
    let names: BTreeSet<String> = vault
        .list()
        .unwrap()
        .into_iter()
        .map(|item| item.name.to_string())
        .collect();
    let expected: BTreeSet<String> = (0..25)
        .flat_map(|n| [format!("a-{n}"), format!("b-{n}")])
        .collect();
    assert_eq!(names, expected);
    fs::remove_dir_all(dir).unwrap();
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn writes_in(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(TEMPORARY_PREFIX)
        })
        .collect()
}

#[test]
fn a_writer_killed_in_the_middle_of_a_write_leaves_the_last_whole_vault() {
    let (dir, _) = writer_dir();
    let (mut kept, mut cut) = (0, 0);
    for round in 0.. {
        if round >= 12 && cut > 0 {
            break;
        }
        assert!(round < 200, "no kill landed in the middle of a write");
        let writer = Running(start_writer(&dir, "k", "forever"));
        std::thread::sleep(Duration::from_millis(10 * (round % 12)));
        let deadline = Instant::now() + Duration::from_secs(30);
        while writes_in(&dir).is_empty() {
            assert!(Instant::now() < deadline, "the writer never wrote");
        }
        drop(writer);
        let orphans = writes_in(&dir);
        cut += usize::from(!orphans.is_empty());
        for orphan in orphans {
            fs::remove_file(orphan).unwrap();
        }
        let vault = Vault::open(&writer_config(&dir), &SandboxView::new([]))
            .expect("a killed write left a vault that does not open");
        let names: BTreeSet<String> = vault
            .list()
            .unwrap()
            .into_iter()
            .map(|item| item.name.to_string())
            .collect();
        let whole: BTreeSet<String> = (0..names.len()).map(|n| format!("k-{n}")).collect();
        assert_eq!(names, whole, "round {round}");
        assert!(names.len() >= kept, "round {round} lost items");
        kept = names.len();
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_reader_never_sees_part_of_a_write() {
    let (dir, vault) = writer_dir();
    let writer = Running(start_writer(&dir, "r", "forever"));
    let until = Instant::now() + Duration::from_millis(1_500);
    let mut sizes = BTreeSet::new();
    while Instant::now() < until {
        let listed = vault.list().expect("a read saw part of a write");
        sizes.insert(listed.len());
    }
    drop(writer);
    assert!(sizes.len() > 1, "no write landed while the reads ran");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_key_file_a_sandbox_could_read_is_refused() {
    let mut under_etc = Scratch::new(
        AGENT_YAML,
        "{path: state/vault.enc, key_file: /etc/solos-vault-test/vault.key}",
    );
    under_etc.refused(&["init"], "", "under /etc");
    assert!(!Path::new("/etc/solos-vault-test").exists());
    assert!(!under_etc.root.join("state/vault.enc").exists());

    let mut node = Scratch::new(
        NODE_YAML,
        "{path: work/vault.enc, key_file: keys/vault.key}",
    );
    node.refused(&["init"], "", "which a sandbox can read");
    let mut mounted = Scratch::new(NODE_YAML, "{path: vault.enc, key_file: data/vault.key}");
    fs::create_dir_all(mounted.root.join("data")).unwrap();
    mounted.refused(&["init"], "", "which a sandbox can read");
    assert!(!mounted.root.join("data/vault.key").exists());

    let mut hidden = Scratch::new(NODE_YAML, "{path: vault.enc, key_file: keys/vault.key}");
    hidden.ok(&["init"], "");
    hidden.ok(
        &[
            "add-key",
            "--name",
            "openai",
            "--host",
            "api.openai.com",
            "--header",
            "authorization",
            "--scheme",
            "bearer",
        ],
        KEY,
    );
    assert!(hidden.ok(&["list"], "").contains("openai"));
    let key = hidden.root.join("keys/vault.key");
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    hidden.refused(&["list"], "", "open to group or others");

    for key_file in [
        "keys/../repo/vault.key",
        "ROOT/claude/vault.key",
        "ROOT/node/lib/vault.key",
    ] {
        let mut agent = Scratch::new(
            AGENT_PI_YAML,
            &format!("{{path: vault.enc, key_file: {key_file}}}"),
        );
        fs::create_dir_all(agent.root.join("keys")).unwrap();
        agent.refused(&["init"], "", "which a sandbox can read");
        assert!(!agent.root.join("repo/vault.key").exists(), "{key_file}");
    }
    let mut agent = Scratch::new(AGENT_PI_YAML, VAULT);
    agent.ok(&["init"], "");
    assert!(agent.ok(&["list"], "").is_empty());
}
