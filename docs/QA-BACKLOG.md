# Verification and QA backlog

Status: proposed. This file is the story text for the delivery map in `docs/TRD.md` §9. The requirements are in §§6.11 and 6.15–6.19 and in §7 (SEC-F-48, SEC-NF-29). Story IDs are suggested backlog IDs. Estimates wait for the repository audit (QA-00).

All commands, paths, profile names and schemas in this file and in the TRD are **proposed interfaces**. None is implemented. Do not describe one as working until it has run. Reuse the repository's existing names and abstractions where they already solve the problem.

**Scope.** The first milestone is a repeatable release experiment with useful evidence and safe remote lifecycle control. It is not exhaustive assurance. Do not broaden it into a runtime rewrite.

**Delivery order.** Work in reviewable increments:

1. TRD change and gap map.
2. Runner plus one deterministic scenario.
3. Immutable bundle and remote lifecycle.
4. One live ten-agent Goal.
5. Three-family catalog and protected release gate.
6. Tested agent instructions.

Parallelize only independent slices, and only after the run, result and fixture contracts are agreed.

## P0 stories

### QA-00: Audit the repository and amend the TRD

Priority P0. Dependencies: none. Requirements: all families.

As an implementation agent, I identify the actual CLI and operator surfaces, the build and release pipeline, provider adapters, fixtures, telemetry, the audit verifier and the deployment scripts. I map the existing requirements to available behavior and to explicit gaps.

**Acceptance.** A revised editable TRD and a focused diff and change log. The existing requirement IDs are preserved. The release and QA requirements and the story mapping are added. Owner decisions are distinguished from proposals. The open contradictions are resolved or tracked (`docs/TRD-CHANGES.md`). No unimplemented command is claimed to work.

**Status.** The TRD amendment is delivered for review. The repository at `45ae3f4` holds only `README.md` and `docs/TRD.md`. There is no code, CLI, CI or deployment script to map, so the gap map is the whole implementation.

### QA-01: QA runner and result contract

Priority P0. Dependencies: QA-00. Requirements: R-VER-1 to R-VER-3, R-QA-1, R-EVAL-1.

As a developer, I run one named scenario and receive a stable JSON result, an exit code and an evidence path.

**Acceptance.** The common run, report and evaluation metadata schema. `PASS`, `FAIL` and `INCONCLUSIVE`. Deadlines and artifact retention. A deliberately failing assertion gives `FAIL`. Missing evidence gives `INCONCLUSIVE`. An empty required selection cannot pass. The same scenario executor runs under the local and remote transports. No dashboard is required.

### QA-02: Prepare and inspect reusable application states

Priority P0. Dependencies: QA-01 and the required domain APIs. Requirements: R-QA-2, R-QA-3, R-PRV-7, SEC-F-48.

As a coding agent, I prepare `task-open` and `budget-nearly-exhausted`, verify readiness, exercise the CLI and clean up without editing the database by hand.

**Acceptance.** Setup uses supported authorization paths. Fixtures have independent IDs and an expiry. A setup failure stops the scenario. A claim-race check and a denied-provider-call check exist, with independent observation at the receiver. A permitted control request proves that the receiver probe works. A repeated cleanup affects only owned state and preserves evidence.

### QA-03: Immutable, relocatable QA bundle

Priority P0. Dependencies: QA-01. Requirements: R-RUN-3, R-REL-2.

As a release operator, I build once and run the same candidate on another compatible Linux machine.

**Acceptance.** A manifest with the runtime, test and fixture digests and the target requirements. The necessary assets, and optional Nextest archives. Remote validation of version, architecture, dependencies and paths. A mismatched artifact or a missing fixture stops the run before live spend. Execution from a different directory without rebuilding the runtime. Offline staging needs no download on the target.

### QA-04: Run, reconnect and stop over SSH

Priority P0. Dependencies: QA-02, QA-03. Requirements: R-RUN-1, R-RUN-2, R-RUN-4.

As an operator, I stage a bundle, start one run, disconnect, reconnect, get its status, collect artifacts and cancel it through the same CLI.

**Acceptance.** A verified host inventory. A stable run ID. A duplicate start returns the same job. A host-side deadline stops all owned worker services. Scoped cleanup and a cleanup report. Demonstrate a lost SSH session, a repeated start after a lost acknowledgement, and expiry with the original controller unavailable. No secret appears in the artifacts. Incomplete evidence is preserved when a host cannot be contacted.

### QA-05: Live ten-agent Goal catalog

Priority P0. Dependencies: QA-02, QA-04, and the real agent loop and Provider and broker path. Requirements: R-REL-1, R-REL-2, R-EVAL-2, R-PRV-7.

As a release operator, I run ten agents against one recorded, randomly selected Goal and obtain an independently checked result.

**Acceptance.** Start with record reconciliation, then add the other two families. The selected case is frozen before inference. Ten identities make attributable contributions with real model calls. The combined artifact is produced and checked. Nine idle agents plus one productive agent fails. A mocked-response run is labeled and cannot satisfy the live gate. The seed, catalog, model settings, accounting and output are preserved, failures included.

### QA-06: Verify actions and explain operational state

Priority P0. Dependencies: QA-01 and the relevant intent, ledger and audit interfaces. Requirements: R-VER-1 to R-VER-3, R-OBS-1 to R-OBS-3.

As a developer or auditor, I inspect a Task or an intent and understand its state without rerunning it.

**Acceptance.** Correlate at least the registration, delegation, provider, settlement and stop paths. Expose authorization, accounting, audit and outcome evidence separately. A missing external receipt is inconclusive, not successful. `explain` returns a recorded reason and the state revision. Wrong-receipt, omitted-event and incomplete-evidence cases show that the verifier detects them.

### QA-07: Candidate-before-release qualification

Priority P0. Dependencies: QA-05, QA-06. Requirements: R-REL-3, R-REL-4, SEC-F-48, SEC-NF-29.

As a maintainer, I prevent a runtime release unless the exact candidate passes its required targeted checks and the live small-swarm run.

**Acceptance.** A protected release policy. Recorded digests of the tested artifacts and configuration. No rebuild between test and promotion. No silent skip when infrastructure or the provider is unavailable. Explicit deferred coverage. A stale report, an altered binary, a failed Goal or an unknown cleanup status blocks promotion. Retries keep every attempt and the same selected case. Early builds made before this gate exists stay unpromoted.

### QA-08: Tested verification skill and feature map

Priority P0, after the commands exist. Dependencies: QA-02, QA-04, QA-06. Requirement: R-VER-1.

As a coding agent in a cold checkout, I find out how to launch, diagnose, drive, verify and clean up the app without reconstructing the workflow.

**Acceptance.** Repository-local instructions use the actual commands. They map the first three to five user-facing features to state recipes and observable results. The instructions are executed end to end, including remote reconnection and the survival of artifacts after cleanup. Use pstack's generator and maintenance workflow when it is installed. Direct CLI operation stays independent of pstack.

## P1 stories

### QA-09: Expand fault and compatibility scenarios

Priority P1. Dependencies: QA-04, QA-06. Requirements: R-RUN-*, `docs/TRD.md` §6.13 criteria 3, 4 and 12.

As a maintainer, I reproduce restart, retry, stale-fencing and cross-node failures in disposable deployments.

**Acceptance.** One mid-intent crash case and one node-loss or partition case, with receiver-side duplicate checks and cleanup. Add prior-state upgrade checks when persistence or migration support ships. Do not promise automatic rollback across incompatible state migrations. Scale selected scenarios toward the §6.13 and §7.19 acceptance suites. Do not change their success criteria.

### QA-10: Repeatable evaluation campaigns

Priority P1. Dependencies: QA-05 and the QA-01 result schema. Requirements: R-EVAL-1, R-EVAL-2, R-ATT-5.

As a researcher, I compare versions, models, agent counts and planning strategies on versioned cases under matched budgets.

**Acceptance.** Fixed and held-out cases are separate. Scorer versions are independent. Goal trials are repeated. Failure accounting is complete. Results are per dimension, with cost and latency summaries. Demonstrate a one-agent against ten-agent comparison and the history-fed against single-draft comparison (§10 criterion 19). Produce an adoption decision for Inspect, or keep a native driver with the same exports.

### QA-11: Extend the action map to remaining app surfaces

Priority P1, or P0 when a new surface ships. Dependencies: QA-02, QA-08. Requirements: R-VER-1, R-MOD-2, SEC-F-44.

As an operator, I verify more CLI and API behavior, and any real UI or TUI, with the same prepared states and evidence contract.

**Acceptance.** Inspect the real interface before choosing a harness. Test the visible results and the authoritative effects. Add module unload and Finding retraction scenarios when they apply. Use a PTY or browser harness only for an interface that exists. New released behavior has a named verification path or an explicit limitation that the release policy handles.

## First-delivery completion checklist

The checklist is `docs/TRD.md` §10 criterion 23. Before the first delivery is called complete, demonstrate:

- one remote run on the exact candidate, with ten contributing agents and an independently checked artifact;
- one deliberate failure that blocks qualification;
- a lost-SSH reconnect without duplicate work;
- a deadline and a cancellation cleanup;
- evidence preserved after cleanup.

Include the command, the environment and the actual report for each demonstration.

**Expected repository deliverables.** The amended TRD, the change log and the requirement-to-scenario map, the accepted stories, the CLI and runner changes, the initial scenario and catalog definitions, a remote inventory example without credentials, the release-policy integration, evidence schema and examples, and the tested verification skill. Mark every example as an example until actual runs replace it.
