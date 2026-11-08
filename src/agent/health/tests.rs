use super::{
    Alerts, ClaudeInstall, ClaudeLogin, HOLD_FILE, Kind, PAUSE_FILE, Probe, Report, Run,
    STARTUP_GRACE_SECS, STATE_FILE, Settings, State, UnitState, alert_target, claude_install,
    free_bytes, hello, output_within, parse_claude_status, parse_unit, restart_check, run,
};
use crate::agent::AgentConfig;
use crate::agent::ports::BoxFuture;
use anyhow::{Result, anyhow};
use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const GIB: u64 = 1 << 30;
const T0: u64 = 1_800_000_000_000;
const MINUTE: u64 = 60_000;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agent-health-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A unit, a broker, a disk and a Claude Code login that the test sets.
struct Fake {
    unit: RefCell<Result<UnitState, String>>,
    broker: RefCell<Result<(), String>>,
    free: Cell<u64>,
    /// `None`: the config names no `claude`, so nothing is asked.
    claude: RefCell<Option<Result<ClaudeLogin, String>>>,
}

fn running() -> UnitState {
    UnitState {
        load_state: "loaded".into(),
        active_state: "active".into(),
        sub_state: "running".into(),
        restarts: Some(0),
        uptime_secs: Some(3_600),
    }
}

impl Fake {
    fn healthy() -> Self {
        Self {
            unit: RefCell::new(Ok(running())),
            broker: RefCell::new(Ok(())),
            free: Cell::new(50 * GIB),
            claude: RefCell::new(None),
        }
    }
    fn stopped(&self) {
        *self.unit.borrow_mut() = Ok(UnitState {
            active_state: "inactive".into(),
            sub_state: "dead".into(),
            restarts: Some(0),
            ..running()
        });
    }
    fn up(&self) {
        *self.unit.borrow_mut() = Ok(running());
    }
}

impl Probe for Fake {
    fn unit(&self) -> Result<UnitState> {
        self.unit.borrow().clone().map_err(|why| anyhow!(why))
    }
    fn broker(&self) -> Result<()> {
        self.broker.borrow().clone().map_err(|why| anyhow!(why))
    }
    fn free_bytes(&self) -> Result<u64> {
        Ok(self.free.get())
    }
    fn claude_login(&self) -> Option<Result<ClaudeLogin>> {
        let login = self.claude.borrow().clone()?;
        Some(login.map_err(|why| anyhow!(why)))
    }
}

/// A Discord that records what is posted.
#[derive(Default)]
struct Chat {
    posts: Mutex<Vec<String>>,
    unreachable: Mutex<bool>,
    refuses_posts: Mutex<bool>,
}

impl Chat {
    fn posts(&self) -> Vec<String> {
        self.posts.lock().unwrap().clone()
    }
    fn set_unreachable(&self, down: bool) {
        *self.unreachable.lock().unwrap() = down;
    }
    fn set_refusing(&self, refuses: bool) {
        *self.refuses_posts.lock().unwrap() = refuses;
    }
}

impl Alerts for Arc<Chat> {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        let down = *self.unreachable.lock().unwrap();
        Box::pin(async move {
            if down {
                Err(anyhow!("discord GET /channels/{{id}} failed"))
            } else {
                Ok(())
            }
        })
    }
    fn post(&self, text: &str) -> BoxFuture<'_, Result<()>> {
        let (down, refuses) = (
            *self.unreachable.lock().unwrap(),
            *self.refuses_posts.lock().unwrap(),
        );
        let text = text.to_owned();
        Box::pin(async move {
            if down || refuses {
                return Err(anyhow!("discord is not answering"));
            }
            self.posts.lock().unwrap().push(text);
            Ok(())
        })
    }
}

fn settings() -> Settings {
    Settings {
        unit: "solos-agent".into(),
        host: "remote-ai".into(),
        min_free_bytes: 5 * GIB,
        resume_free_bytes: 8 * GIB,
        restart_burst: 3,
    }
}

async fn check(
    scratch: &Scratch,
    fake: &Fake,
    chat: Option<&Arc<Chat>>,
    at: u64,
    dry_run: bool,
) -> Report {
    let alerts: Option<&dyn Alerts> = chat.map(|chat| chat as &dyn Alerts);
    run(&Run {
        state_dir: scratch.path(),
        settings: &settings(),
        probe: fake,
        alerts,
        target: None,
        config_error: None,
        dry_run,
        now_ms: at,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_healthy_agent_is_never_mentioned() {
    let scratch = Scratch::new("healthy");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    for minute in 0..5 {
        let report = check(
            &scratch,
            &fake,
            Some(&chat),
            T0 + minute * 3 * MINUTE,
            false,
        )
        .await;
        assert!(report.ok, "{report:?}");
        assert_eq!(report.posted, None);
    }
    assert!(chat.posts().is_empty());
    assert_eq!(State::load(scratch.path()).incident, None);
}

#[tokio::test]
async fn a_down_agent_is_told_once_per_incident_and_its_return_once() {
    let scratch = Scratch::new("incident");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    check(&scratch, &fake, Some(&chat), T0, false).await;

    fake.stopped();
    let first = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert!(!first.ok);
    assert_eq!(first.posted, Some("down"));
    for minute in [6, 9, 12] {
        let again = check(&scratch, &fake, Some(&chat), T0 + minute * MINUTE, false).await;
        assert_eq!(again.posted, None, "told already");
        assert_eq!(again.quiet, Some("already told"));
    }
    let posts = chat.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(posts[0].starts_with("solos-agent health check failed on remote-ai:"));
    assert!(
        posts[0].contains("solos-agent is inactive (dead)"),
        "{}",
        posts[0]
    );
    assert!(posts[0].contains("outside the agent"));
    assert!(posts[0].len() < 2_000);

    fake.up();
    let back = check(&scratch, &fake, Some(&chat), T0 + 15 * MINUTE, false).await;
    assert!(back.ok);
    assert_eq!(back.posted, Some("recovered"));
    assert!(
        chat.posts()[1].contains("healthy again after 12m"),
        "{:?}",
        chat.posts()
    );
    check(&scratch, &fake, Some(&chat), T0 + 18 * MINUTE, false).await;
    assert_eq!(chat.posts().len(), 2, "nothing more once it is well");

    fake.stopped();
    let next = check(&scratch, &fake, Some(&chat), T0 + 21 * MINUTE, false).await;
    assert_eq!(next.posted, Some("down"), "a new incident is told again");
    assert_eq!(chat.posts().len(), 3);
}

#[tokio::test]
async fn a_new_kind_of_trouble_in_the_same_incident_is_told_and_the_same_kind_is_not() {
    let scratch = Scratch::new("kinds");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    *fake.broker.borrow_mut() = Err("no answer in 5 s".into());
    let first = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert_eq!(first.posted, Some("down"));
    assert!(chat.posts()[0].contains("the broker socket does not answer: no answer in 5 s"));

    fake.free.set(GIB);
    let second = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert_eq!(second.posted, Some("down"), "the disk is news");
    assert!(chat.posts()[1].contains("only 1.0 GB free"));
    assert!(
        chat.posts()[1].contains("the broker socket"),
        "and it lists what else fails"
    );
    let third = check(&scratch, &fake, Some(&chat), T0 + 6 * MINUTE, false).await;
    assert_eq!(third.posted, None);
    assert_eq!(chat.posts().len(), 2);
}

#[tokio::test]
async fn a_post_that_fails_is_tried_again_and_told_once_when_it_goes() {
    let scratch = Scratch::new("retry");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    fake.stopped();
    chat.set_refusing(true);
    let failed = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert_eq!(failed.posted, None);
    assert!(
        failed
            .post_error
            .as_deref()
            .unwrap()
            .contains("not answering")
    );
    assert_eq!(
        State::load(scratch.path())
            .incident
            .unwrap()
            .announced
            .len(),
        0
    );

    chat.set_refusing(false);
    let sent = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert_eq!(sent.posted, Some("down"));
    let quiet = check(&scratch, &fake, Some(&chat), T0 + 6 * MINUTE, false).await;
    assert_eq!(quiet.posted, None);
    assert_eq!(chat.posts().len(), 1);
}

#[tokio::test]
async fn when_discord_is_the_only_trouble_there_is_nobody_to_tell() {
    let scratch = Scratch::new("discord");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    chat.set_unreachable(true);
    let report = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert!(!report.ok);
    assert_eq!(report.posted, None);
    assert!(
        report
            .post_error
            .as_deref()
            .unwrap()
            .contains("nowhere to tell")
    );
    assert!(report.checks.iter().any(|c| c.name == "discord" && !c.ok));

    // Discord comes back and the agent never went down: no incident was told,
    // so there is no recovery to tell either.
    chat.set_unreachable(false);
    let back = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert!(back.ok);
    assert!(chat.posts().is_empty());
}

#[tokio::test]
async fn an_agent_down_while_discord_is_too_is_told_as_soon_as_discord_answers() {
    let scratch = Scratch::new("both");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    fake.stopped();
    chat.set_unreachable(true);
    let first = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert!(first.post_error.is_some() && first.posted.is_none());
    chat.set_unreachable(false);
    let second = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert_eq!(second.posted, Some("down"));
    assert!(
        chat.posts()[0].contains("Since 2027-01-15T08:00:00Z"),
        "it says since when it has been down, not when it was told: {}",
        chat.posts()[0]
    );
}

#[tokio::test]
async fn a_hold_records_trouble_and_tells_nothing_until_it_expires() {
    let scratch = Scratch::new("hold");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    std::fs::write(
        scratch.path().join(HOLD_FILE),
        format!("{}\n", T0 / 1_000 + 20 * 60),
    )
    .unwrap();
    fake.stopped();
    let held = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert_eq!(held.posted, None);
    assert!(held.quiet.unwrap().starts_with("held"));
    assert!(State::load(scratch.path()).incident.is_some());

    let expired = check(&scratch, &fake, Some(&chat), T0 + 21 * MINUTE, false).await;
    assert_eq!(
        expired.posted,
        Some("down"),
        "a deploy that went wrong is told"
    );

    // A deploy that went right leaves no trace.
    let scratch = Scratch::new("hold-clean");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    std::fs::write(
        scratch.path().join(HOLD_FILE),
        format!("{}", T0 / 1_000 + 20 * 60),
    )
    .unwrap();
    fake.stopped();
    check(&scratch, &fake, Some(&chat), T0, false).await;
    fake.up();
    check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert!(chat.posts().is_empty());
    assert_eq!(State::load(scratch.path()).incident, None);
}

#[test]
fn restarts_are_counted_since_the_last_look_and_a_reset_starts_over() {
    let settings = settings();
    let unit = |restarts| UnitState {
        restarts,
        ..running()
    };
    assert_eq!(
        restart_check(&settings, &unit(Some(40)), None),
        (None, Some(40)),
        "the first look only records"
    );
    assert_eq!(
        restart_check(&settings, &unit(Some(42)), Some(40)),
        (None, Some(42)),
        "two restarts in a window are not a loop"
    );
    let (looping, seen) = restart_check(&settings, &unit(Some(45)), Some(42));
    assert!(looping.unwrap().contains("restarted itself 3 times"));
    assert_eq!(seen, Some(45));
    assert_eq!(
        restart_check(&settings, &unit(Some(1)), Some(45)),
        (None, Some(1)),
        "a manual restart reset the count"
    );
    assert!(
        restart_check(&settings, &unit(Some(3)), Some(45))
            .0
            .is_some()
    );
    assert_eq!(
        restart_check(&settings, &unit(None), Some(7)),
        (None, Some(7))
    );
}

#[tokio::test]
async fn a_unit_that_restarts_itself_over_and_over_is_a_crash_loop_even_while_it_looks_active() {
    let scratch = Scratch::new("loop");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    check(&scratch, &fake, Some(&chat), T0, false).await;
    fake.unit.borrow_mut().as_mut().unwrap().restarts = Some(4);
    let report = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert_eq!(report.posted, Some("down"));
    assert!(
        chat.posts()[0].contains("crash-looping"),
        "{:?}",
        chat.posts()
    );

    // Waiting out systemd's restart delay is the same trouble, said as it is.
    *fake.unit.borrow_mut() = Ok(UnitState {
        active_state: "activating".into(),
        sub_state: "auto-restart".into(),
        ..running()
    });
    let waiting = check(&scratch, &fake, Some(&chat), T0 + 6 * MINUTE, false).await;
    assert!(
        waiting
            .checks
            .iter()
            .any(|c| c.name == "unit" && !c.ok && c.detail.contains("systemd is restarting it"))
    );
}

#[tokio::test]
async fn the_broker_is_asked_only_of_a_unit_that_has_had_time_to_start() {
    let scratch = Scratch::new("grace");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    *fake.broker.borrow_mut() = Err("cannot connect".into());
    fake.unit.borrow_mut().as_mut().unwrap().uptime_secs = Some(STARTUP_GRACE_SECS - 1);
    let young = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert!(young.ok, "{young:?}");

    fake.unit.borrow_mut().as_mut().unwrap().uptime_secs = Some(STARTUP_GRACE_SECS);
    let old = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert_eq!(old.posted, Some("down"));

    // A stopped unit has no socket to ask about; the unit is the news.
    let scratch = Scratch::new("grace-stopped");
    let fake = Fake::healthy();
    *fake.broker.borrow_mut() = Err("cannot connect".into());
    fake.stopped();
    let report = check(&scratch, &fake, None, T0, false).await;
    assert!(report.checks.iter().all(|c| c.name != "broker"));
}

#[tokio::test]
async fn low_disk_pauses_new_work_alerts_and_lifts_the_pause_it_made_with_room_to_spare() {
    let scratch = Scratch::new("disk");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    fake.free.set(4 * GIB);
    let low = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert_eq!(low.pause, Some("paused"));
    assert_eq!(low.posted, Some("down"));
    let pause = std::fs::read_to_string(scratch.path().join(PAUSE_FILE)).unwrap();
    assert!(
        pause.starts_with("paused by solos-agent-health: 4.0 GB free"),
        "{pause}"
    );
    assert!(chat.posts()[0].contains("only 4.0 GB free"));
    assert!(chat.posts()[0].contains("New work is paused"));
    assert!(State::load(scratch.path()).disk_paused);

    // Back over the floor but not over the resume level: still paused, no flapping.
    fake.free.set(6 * GIB);
    let between = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert!(!between.ok && between.pause.is_none());
    assert!(scratch.path().join(PAUSE_FILE).exists());
    assert_eq!(chat.posts().len(), 1, "the same kind: told once");

    fake.free.set(9 * GIB);
    let freed = check(&scratch, &fake, Some(&chat), T0 + 6 * MINUTE, false).await;
    assert!(freed.ok);
    assert_eq!(freed.pause, Some("resumed"));
    assert!(!scratch.path().join(PAUSE_FILE).exists());
    assert!(!State::load(scratch.path()).disk_paused);
    assert_eq!(freed.posted, Some("recovered"));
}

#[tokio::test]
async fn a_pause_a_person_made_is_never_overwritten_or_lifted() {
    let scratch = Scratch::new("foreign");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    std::fs::write(scratch.path().join(PAUSE_FILE), "looking at spend\n").unwrap();
    fake.free.set(GIB);
    let low = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert_eq!(low.pause, None);
    assert_eq!(
        std::fs::read_to_string(scratch.path().join(PAUSE_FILE)).unwrap(),
        "looking at spend\n"
    );
    assert!(!State::load(scratch.path()).disk_paused);
    fake.free.set(50 * GIB);
    check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert!(scratch.path().join(PAUSE_FILE).exists(), "theirs stays");

    // And one we wrote, replaced by a person's, is theirs too.
    let scratch = Scratch::new("replaced");
    fake.free.set(GIB);
    check(&scratch, &fake, Some(&chat), T0, false).await;
    std::fs::write(scratch.path().join(PAUSE_FILE), "mine now\n").unwrap();
    fake.free.set(50 * GIB);
    check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert!(scratch.path().join(PAUSE_FILE).exists());
}

#[tokio::test]
async fn a_floor_of_zero_never_pauses() {
    let scratch = Scratch::new("nofloor");
    let fake = Fake::healthy();
    fake.free.set(1);
    let mut off = settings();
    off.min_free_bytes = 0;
    let report = run(&Run {
        state_dir: scratch.path(),
        settings: &off,
        probe: &fake,
        alerts: None,
        target: None,
        config_error: None,
        dry_run: false,
        now_ms: T0,
    })
    .await
    .unwrap();
    assert!(report.ok && report.pause.is_none());
    assert!(!scratch.path().join(PAUSE_FILE).exists());
}

#[tokio::test]
async fn a_dry_run_looks_and_changes_nothing() {
    let scratch = Scratch::new("dry");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    fake.stopped();
    fake.free.set(GIB);
    let report = check(&scratch, &fake, Some(&chat), T0, true).await;
    assert!(!report.ok && report.dry_run);
    assert_eq!(report.posted, None);
    assert!(chat.posts().is_empty());
    assert!(!scratch.path().join(PAUSE_FILE).exists());
    assert!(!scratch.path().join(STATE_FILE).exists());
}

#[tokio::test]
async fn without_a_token_it_still_checks_and_says_why_it_posted_nothing() {
    let scratch = Scratch::new("notoken");
    let fake = Fake::healthy();
    fake.stopped();
    let report = check(&scratch, &fake, None, T0, false).await;
    assert!(!report.ok);
    assert!(
        report
            .post_error
            .as_deref()
            .unwrap()
            .contains("no Discord token")
    );
    assert!(
        report
            .checks
            .iter()
            .any(|c| c.name == "discord" && c.detail.contains("skipped"))
    );
}

#[tokio::test]
async fn a_systemd_that_cannot_be_asked_is_trouble_not_silence() {
    let scratch = Scratch::new("nosystemd");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    *fake.unit.borrow_mut() = Err("no user bus".into());
    let report = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert_eq!(report.posted, Some("down"));
    assert!(chat.posts()[0].contains("cannot ask systemd about solos-agent: no user bus"));
}

#[test]
fn systemctl_show_is_read_into_a_unit_and_its_age() {
    let text = "LoadState=loaded\nActiveState=active\nSubState=running\nNRestarts=2\n\
                ActiveEnterTimestampMonotonic=1000000000\n";
    let unit = parse_unit(text, Some(1_600));
    assert_eq!(
        unit,
        UnitState {
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            restarts: Some(2),
            uptime_secs: Some(600),
        }
    );
    let never = parse_unit(
        "ActiveState=inactive\nActiveEnterTimestampMonotonic=0\n",
        Some(9),
    );
    assert_eq!((never.uptime_secs, never.restarts), (None, None));
}

fn serve_once(socket: &Path, answer: &'static [u8]) -> std::thread::JoinHandle<String> {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut seen = vec![0u8; 512];
        let n = stream.read(&mut seen).unwrap();
        stream.write_all(answer).unwrap();
        String::from_utf8_lossy(&seen[..n]).into_owned()
    })
}

#[test]
fn the_broker_probe_asks_hello_over_the_socket_and_wants_a_200() {
    let scratch = Scratch::new("socket");
    let socket = scratch.path().join("broker.sock");
    let server = serve_once(&socket, b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
    hello(&socket).unwrap();
    let request = server.join().unwrap();
    assert!(
        request.starts_with("GET /api/hello HTTP/1.1\r\n"),
        "{request}"
    );

    let sick = scratch.path().join("sick.sock");
    let server = serve_once(&sick, b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
    let err = hello(&sick).unwrap_err().to_string();
    assert!(err.contains("503"), "{err}");
    server.join().unwrap();

    let err = format!(
        "{:#}",
        hello(&scratch.path().join("gone.sock")).unwrap_err()
    );
    assert!(err.contains("cannot connect"), "{err}");
}

#[test]
fn free_space_is_read_from_the_disk_and_a_missing_directory_is_an_error() {
    let scratch = Scratch::new("space");
    assert!(free_bytes(scratch.path()).unwrap() > 0);
    assert!(free_bytes(&scratch.path().join("nope")).is_err());
}

fn config(extra_source: &str, operator: &str) -> AgentConfig {
    let yaml = format!(
        "policy:\n  people: [discord:1]\nsources:\n  - {extra_source}\nchannels:\n  - kind: discord\n\
         router: {{kind: scripted}}\nexecutor: {{kind: scripted}}\n{operator}"
    );
    let config: AgentConfig = serde_yaml::from_str(&yaml).unwrap();
    config.validate().unwrap();
    config
}

#[test]
fn alerts_go_to_the_watchdogs_discord_channel_on_the_sources_api_root() {
    let discord = "{kind: discord, guilds: ['1'], token_file: t, api_base: 'http://127.0.0.1:9/'}";
    let watchdog = "watchdog: {operator: {source: discord, address: {channel: '42'}}}\n";
    let (channel, base) = alert_target(&config(discord, watchdog)).unwrap();
    assert_eq!(
        (channel.as_str(), base.as_str()),
        ("42", "http://127.0.0.1:9/")
    );

    let plain = "{kind: discord, guilds: ['1'], token_file: t}";
    assert_eq!(
        alert_target(&config(plain, watchdog)).unwrap().1,
        "https://discord.com/api/v10/"
    );
    let none = alert_target(&config(plain, "")).unwrap_err().to_string();
    assert!(none.contains("no watchdog section"), "{none}");
}

#[test]
fn the_incident_the_message_and_its_kinds_survive_a_state_file_round_trip() {
    let scratch = Scratch::new("state");
    let state = State {
        restarts_seen: Some(3),
        incident: Some(super::Incident {
            since_ms: T0,
            announced: [Kind::Unit, Kind::Disk].into(),
        }),
        ..State::default()
    };
    state.save(scratch.path()).unwrap();
    assert_eq!(State::load(scratch.path()), state);
    std::fs::write(scratch.path().join(STATE_FILE), "{not json").unwrap();
    assert_eq!(
        State::load(scratch.path()),
        State::default(),
        "damage costs one repeat, not a crash"
    );
}

#[tokio::test]
async fn a_config_that_no_longer_loads_is_told_and_the_target_it_last_named_is_kept() {
    let scratch = Scratch::new("config");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    let target = super::Target {
        channel: "42".into(),
        api_base: "https://discord.com/api/v10/".into(),
    };
    async fn ask(
        scratch: &Scratch,
        fake: &Fake,
        chat: &Arc<Chat>,
        target: Option<&super::Target>,
        error: Option<&str>,
        at: u64,
    ) -> Report {
        run(&Run {
            state_dir: scratch.path(),
            settings: &settings(),
            probe: fake,
            alerts: Some(chat as &dyn Alerts),
            target,
            config_error: error,
            dry_run: false,
            now_ms: at,
        })
        .await
        .unwrap()
    }
    assert!(
        ask(&scratch, &fake, &chat, Some(&target), None, T0)
            .await
            .ok
    );
    assert_eq!(State::load(scratch.path()).target, Some(target.clone()));

    // The edit that broke it: the agent would not start, and neither would a
    // check that needed the file. The remembered target is what carries this.
    let error = Some("unknown field `wathdog`");
    let broken = ask(&scratch, &fake, &chat, None, error, T0 + 3 * MINUTE).await;
    assert_eq!(broken.posted, Some("down"));
    assert!(
        chat.posts()[0].contains("agent.yaml does not load"),
        "{:?}",
        chat.posts()
    );
    assert!(chat.posts()[0].contains("unknown field `wathdog`"));
    assert_eq!(
        State::load(scratch.path()).target,
        Some(target),
        "a config that does not load does not forget it"
    );
}

const DAY: u64 = 24 * 60 * MINUTE;

fn signed_in(ends_ms: Option<u64>) -> Option<Result<ClaudeLogin, String>> {
    Some(Ok(ClaudeLogin {
        logged_in: true,
        ends_ms,
    }))
}

fn claude_detail(report: &Report) -> Option<(bool, String)> {
    let found = report.checks.iter().find(|check| check.name == "claude");
    found.map(|check| (check.ok, check.detail.clone()))
}

#[tokio::test]
async fn an_ended_claude_code_sign_in_is_told_once_per_incident_and_its_renewal_once() {
    let scratch = Scratch::new("claude");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    *fake.claude.borrow_mut() = signed_in(Some(T0 + 10 * MINUTE));
    let well = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert!(well.ok, "{well:?}");
    assert_eq!(
        claude_detail(&well),
        Some((true, "signed in until 2027-01-15T08:10:00Z".into()))
    );

    let ended = check(&scratch, &fake, Some(&chat), T0 + 12 * MINUTE, false).await;
    assert!(!ended.ok, "an ended sign-in makes `agent health` exit 1");
    assert_eq!(ended.posted, Some("down"));
    for minute in [15, 18, 21] {
        let again = check(&scratch, &fake, Some(&chat), T0 + minute * MINUTE, false).await;
        assert_eq!(again.quiet, Some("already told"));
    }
    let posts = chat.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(
        posts[0].contains("the Claude Code sign-in on this host ended at 2027-01-15T08:10:00Z"),
        "{}",
        posts[0]
    );
    assert!(posts[0].contains("`claude auth login`"), "{}", posts[0]);
    assert_eq!(
        State::load(scratch.path()).incident.unwrap().announced,
        [Kind::Claude].into()
    );

    *fake.claude.borrow_mut() = signed_in(Some(T0 + 7 * DAY));
    let back = check(&scratch, &fake, Some(&chat), T0 + 24 * MINUTE, false).await;
    assert!(back.ok);
    assert_eq!(back.posted, Some("recovered"));
    check(&scratch, &fake, Some(&chat), T0 + 27 * MINUTE, false).await;
    assert_eq!(chat.posts().len(), 2, "nothing more once it is renewed");
}

#[tokio::test]
async fn a_claude_code_that_is_signed_out_or_cannot_be_asked_is_trouble_not_silence() {
    let scratch = Scratch::new("claude-out");
    let (fake, chat) = (Fake::healthy(), Arc::new(Chat::default()));
    let none = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert_eq!(
        claude_detail(&none),
        None,
        "no claude in the config: no check"
    );

    *fake.claude.borrow_mut() = signed_in(None);
    let old = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert_eq!(
        claude_detail(&old),
        Some((true, "signed in".into())),
        "a Claude Code that does not say when it ends is trusted on `loggedIn`"
    );

    *fake.claude.borrow_mut() = Some(Ok(ClaudeLogin {
        logged_in: false,
        ends_ms: None,
    }));
    let out = check(&scratch, &fake, Some(&chat), T0 + 3 * MINUTE, false).await;
    assert_eq!(out.posted, Some("down"));
    assert!(
        chat.posts()[0].contains("Claude Code is not signed in on this host"),
        "{:?}",
        chat.posts()
    );

    let scratch = Scratch::new("claude-hang");
    *fake.claude.borrow_mut() = Some(Err("no answer in 20s".into()));
    let hung = check(&scratch, &fake, Some(&chat), T0, false).await;
    assert!(!hung.ok);
    assert!(
        chat.posts()[1].contains("cannot ask Claude Code about its login: no answer in 20s"),
        "{:?}",
        chat.posts()
    );
}

#[test]
fn claude_auth_status_is_read_for_its_login_and_nothing_else() {
    let signed_in = r#"{"loggedIn": true, "authMethod": "claude.ai", "loginExpiresAt":
        "2026-11-17T00:26:39.036Z", "email": "owner@example.com", "orgName": "Acme"}"#;
    assert_eq!(
        parse_claude_status(signed_in).unwrap(),
        ClaudeLogin {
            logged_in: true,
            ends_ms: Some(1_794_875_199_036),
        }
    );
    assert_eq!(
        parse_claude_status(r#"{"loggedIn": false, "authMethod": "none", "loginExpiresAt": null}"#)
            .unwrap(),
        ClaudeLogin {
            logged_in: false,
            ends_ms: None,
        }
    );
    assert_eq!(
        parse_claude_status(r#"{"loggedIn": true}"#)
            .unwrap()
            .ends_ms,
        None,
        "a Claude Code before 2.1.318 has no end date"
    );
    for (case, text) in [
        (
            "text",
            "Not logged in. Run claude auth login to authenticate.",
        ),
        ("nothing", ""),
        ("no loggedIn", r#"{"authMethod": "claude.ai"}"#),
        (
            "an end it cannot read",
            r#"{"loggedIn": true, "loginExpiresAt": "next week"}"#,
        ),
    ] {
        let why = format!("{:#}", parse_claude_status(text).unwrap_err());
        assert!(!why.contains("owner@example.com"), "{case}: {why}");
        assert!(
            why.contains("loggedIn") || why.contains("loginExpiresAt"),
            "{case}: {why}"
        );
    }
}

/// A `claude` that prints what the test queued, for `auth status --json` only,
/// and writes each call's arguments to `calls`. It is run by `sh`, as an
/// install with `node` is, so no executable is written.
fn fake_claude(scratch: &Scratch, status: &str, exit: u8) -> ClaudeInstall {
    let script = scratch.path().join("claude.sh");
    std::fs::write(
        &script,
        "d=$(dirname \"$0\")\necho \"$*\" >> \"$d/calls\"\n\
         [ \"$*\" = \"auth status --json\" ] || exit 2\n\
         cat \"$d/status.json\"\nexit \"$(cat \"$d/exit\")\"\n",
    )
    .unwrap();
    queue_claude(scratch, status, exit);
    ClaudeInstall {
        binary: script,
        node: Some("/bin/sh".into()),
        required: false,
    }
}

fn queue_claude(scratch: &Scratch, status: &str, exit: u8) {
    std::fs::write(scratch.path().join("status.json"), status).unwrap();
    std::fs::write(scratch.path().join("exit"), exit.to_string()).unwrap();
}

#[test]
fn the_claude_binary_is_asked_only_for_its_status_and_a_signed_out_exit_1_is_still_an_answer() {
    let scratch = Scratch::new("claude-bin");
    let signed = r#"{"loggedIn": true, "loginExpiresAt": "2027-01-01T00:00:00Z"}"#;
    let claude = fake_claude(&scratch, signed, 0);
    let login = claude.ask().unwrap().unwrap();
    assert_eq!(login.ends_ms, Some(1_798_761_600_000));

    queue_claude(&scratch, r#"{"loggedIn": false}"#, 1);
    let out = claude.ask().unwrap().unwrap();
    assert!(
        !out.logged_in,
        "`claude auth status` exits 1 when signed out"
    );

    queue_claude(&scratch, "Segmentation fault", 139);
    let why = format!("{:#}", claude.ask().unwrap().unwrap_err());
    assert!(why.contains("exit status: 139"), "{why}");
    assert!(why.contains("JSON"), "{why}");
    assert!(
        !why.contains("Segmentation"),
        "its output is not echoed: {why}"
    );

    let calls = std::fs::read_to_string(scratch.path().join("calls")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["auth status --json"; 3],
        "no session, no prompt, no other command"
    );
}

#[test]
fn the_native_build_runs_itself_and_the_npm_build_runs_under_node() {
    let native = ClaudeInstall {
        binary: "/opt/claude/claude".into(),
        node: None,
        required: false,
    };
    let command = native.status_command();
    assert_eq!(command.get_program(), "/opt/claude/claude");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["auth", "status", "--json"]
    );
    let npm = ClaudeInstall {
        binary: "/opt/claude/cli.js".into(),
        node: Some("/opt/node/bin/node".into()),
        required: false,
    };
    let command = npm.status_command();
    assert_eq!(command.get_program(), "/opt/node/bin/node");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["/opt/claude/cli.js", "auth", "status", "--json"]
    );
    let env: Vec<_> = command
        .get_envs()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value.is_some()))
        .collect();
    assert!(
        env.contains(&("DISABLE_AUTOUPDATER".into(), true)),
        "asking never updates it: {env:?}"
    );
}

#[test]
fn a_missing_claude_is_skipped_unless_the_config_runs_workers_on_it() {
    let scratch = Scratch::new("claude-missing");
    let gone = scratch.path().join("no-such-claude");
    let optional = ClaudeInstall {
        binary: gone.clone(),
        node: None,
        required: false,
    };
    assert!(
        optional.ask().is_none(),
        "a Plan may name it; nothing to check"
    );
    let required = ClaudeInstall {
        required: true,
        ..optional
    };
    let why = format!("{:#}", required.ask().unwrap().unwrap_err());
    assert!(why.contains(&gone.display().to_string()), "{why}");
    assert!(why.contains("claude-code"), "{why}");

    let node_gone = ClaudeInstall {
        binary: scratch.path().to_owned(),
        node: Some(gone.clone()),
        required: true,
    };
    assert!(format!("{:#}", node_gone.ask().unwrap().unwrap_err()).contains("no-such-claude"));
}

#[test]
fn a_claude_that_hangs_is_killed_and_reported_inside_the_limit() {
    let started = Instant::now();
    let mut sleeper = Command::new("sleep");
    sleeper.arg("30");
    let why = format!(
        "{:#}",
        output_within(sleeper, Duration::from_millis(200)).unwrap_err()
    );
    assert!(why.contains("no answer in 200ms"), "{why}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn the_config_says_where_claude_code_is_and_whether_workers_run_on_it() {
    let load = |executor: &str| {
        let yaml = format!(
            "policy:\n  people: [slack:alice]\nsources:\n  - kind: jsonl\n    path: t.jsonl\n\
             channels:\n  - kind: jsonl\n    source: slack\n    path: o.jsonl\n\
             router: {{kind: scripted}}\nexecutor: {executor}\n"
        );
        let config: AgentConfig = serde_yaml::from_str(&yaml).unwrap();
        config.validate().unwrap();
        config
    };
    let pi = "\n  kind: pi\n  provider: deepseek\n  model: deepseek-flash\n  prices: \
              {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}";
    assert_eq!(claude_install(&load("{kind: scripted}")), None);
    assert_eq!(claude_install(&load(pi)), None, "Pi workers, no claude");
    let install = |extra: &str| claude_install(&load(&format!("{pi}\n{extra}")));
    assert_eq!(
        install("  claude: {binary: /opt/claude/claude}"),
        Some(ClaudeInstall {
            binary: "/opt/claude/claude".into(),
            node: None,
            required: false,
        }),
        "a Plan may still name it, so it is checked where it is installed"
    );
    assert_eq!(
        install("  harness: claude-code\n  claude: {binary: /opt/cli.js, node: /opt/node}"),
        Some(ClaudeInstall {
            binary: "/opt/cli.js".into(),
            node: Some("/opt/node".into()),
            required: true,
        })
    );
}
