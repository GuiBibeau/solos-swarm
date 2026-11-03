//! `agent bench` with the scripted Router on a small, fast workload, and
//! the refusal of a paid run without a spend cap. No test here calls a
//! model provider.

use solos_agent_store::SqliteStore;
use solos_swarm::agent::STORE_FILE;
use solos_swarm::agent::bench::workload::{Span, WorkloadSpec};
use solos_swarm::agent::bench::{self, BenchOptions, BenchRouter, Design};
use solos_swarm::agent::simulated::TimeFactor;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("agent-bench-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Five overlapping requests in four workload minutes, each with two to
/// three minutes of work, at a hundredth of real time. Chatter starts its
/// own Thread every time, so the Router's chatter decisions do not depend
/// on how the Source batches lines.
fn small() -> WorkloadSpec {
    WorkloadSpec {
        seed: 11,
        messages: 40,
        requests: 5,
        follow_ups: 8,
        ci_messages: 5,
        per_minute: 10.0,
        work_minutes: Span { min: 2.0, max: 3.0 },
        reply_share: 0.0,
        time_factor: TimeFactor::new(0.01).unwrap(),
        ..WorkloadSpec::default()
    }
}

/// Router decisions that ignored a follow-up its Ticket already had.
fn follow_up_decisions(out: &Path, design: Design) -> usize {
    let path = out.join(design.name()).join("run-1/state").join(STORE_FILE);
    SqliteStore::open_read_only(path)
        .unwrap()
        .decisions()
        .unwrap()
        .iter()
        .filter(|d| d.eval.is_some() && d.reason.starts_with("a follow-up on"))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scripted_bench_runs_every_design_and_shows_what_each_one_changes() {
    let scratch = Scratch::new("designs");
    let out = scratch.0.join("out");
    let options = BenchOptions {
        workload: small(),
        designs: Design::ALL.to_vec(),
        runs: 1,
        out: out.clone(),
        deadline_minutes: None,
        router: BenchRouter::Scripted,
    };
    let report = bench::run(options).await.unwrap();

    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("report.json")).unwrap()).unwrap();
    let markdown = std::fs::read_to_string(out.join("report.md")).unwrap();
    assert_eq!(json["designs"].as_array().unwrap().len(), 3);
    assert!(markdown.contains("| Design | Completion p50 / p95 (min) |"));
    for design in &report.designs {
        assert!(markdown.contains(&format!("| {} |", design.design)));
        let [run] = &design.runs[..] else {
            panic!("{:?}", design.runs)
        };
        assert_eq!(run.triggers_fed, 40, "{}: the whole stream", design.design);
        assert_eq!(run.requests.len(), 5, "{}", design.design);
        assert!(
            run.unfinished.is_empty(),
            "{}: {:?}",
            design.design,
            run.unfinished
        );
        assert!(run.requests.iter().all(|r| r.completion_minutes.is_some()));
        assert_eq!(run.other_tickets, 0, "only requests become work");
        assert_eq!((run.model_calls, run.spend_microusd), (0, 0), "free");
        assert!(run.router_decisions > 0 && run.evals >= run.router_decisions);
        assert!(out.join(&run.dir).join("agent.yaml").is_file());
        assert!(out.join(&run.dir).join("outbox.jsonl").is_file());
    }
    let [super_agent, no_pass_through, serial] = &report.designs[..] else {
        panic!()
    };
    assert_eq!(follow_up_decisions(&out, Design::SuperAgent), 0);
    assert!(follow_up_decisions(&out, Design::NoPassThrough) > 0);
    assert!(
        no_pass_through.router_decisions > super_agent.router_decisions,
        "follow-ups reach the Router: {} against {}",
        no_pass_through.router_decisions,
        super_agent.router_decisions
    );
    let (p95, serial_p95) = (
        super_agent.p95_minutes.unwrap(),
        serial.p95_minutes.unwrap(),
    );
    assert!(p95 < 3.5, "parallel work takes about its own time: {p95}");
    assert!(
        serial_p95 > 2.0 * p95,
        "serial work waits for the work before it: {serial_p95} against {p95}"
    );
}

#[test]
fn a_paid_bench_refuses_to_start_without_a_spend_cap() {
    let scratch = Scratch::new("paid");
    let key = scratch.0.join("key");
    std::fs::write(&key, "not-a-key").unwrap();
    let out = scratch.0.join("out");
    let output = Command::new(env!("CARGO_BIN_EXE_solos-swarm"))
        .args([
            "agent",
            "bench",
            "--design",
            "super-agent",
            "--router",
            "pi",
        ])
        .arg("--out")
        .arg(&out)
        .arg("--router-config")
        .arg(scratch.0.join("agent.yaml"))
        .arg("--pi-root")
        .arg(&scratch.0)
        .arg("--node")
        .arg(&scratch.0)
        .arg("--credential-file")
        .arg(&key)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--max-spend-usd"), "{stderr}");
    assert!(key.is_file(), "the key was never read");
    assert!(!out.exists(), "nothing started");
}
