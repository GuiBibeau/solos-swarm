Historical TRD amendment log. Not an operator runbook. Normative text: [../TRD.md](../TRD.md).

# TRD change log: verification, remote QA and release qualification

Base: PR #1, branch `trd-v1`, commit `45ae3f4`. Source of the change: `solos-swarm-verification-research-handoff.md` (prepared 2026-10-12). Status: proposed amendment for review. It is not committed.

## 1. Owner decisions and proposals

**Owner requirements (decided).** They are the six items at the start of `docs/TRD.md` §6.19: a small-swarm test before every release; ten real, LLM-driven agents under one Goal; a Goal that varies between releases; remote deployment over SSH or an equivalent; short commands for developers and coding agents; and no full suite on every iteration.

**Everything else is proposed.** That includes the three catalog families, the 4/3/3 topology, the ten-minute and twenty-minute calibration values, the exit-status mapping, the profile names and every command name. The proposals depend on Q27 to Q30 and on PoC measurements.

## 2. Changes by TRD location

| Handoff amendment row | Change |
|---|---|
| §§3 and 10: scope and delivery | §3 adds two goals and two non-goals. §2 adds the maintainer user. §10 adds the release-qualification scope, two out-of-scope items, a separation statement and criteria 23 and 24. The 500-agent obligations are unchanged. |
| §6.9: intents and recovery | New bullet "Verification evidence": the intent record is the one source of execution truth, and evidence links to it. |
| §6.11: observability | Correlation identifiers, read-only diagnostic queries, evidence independent of sampling and R-OBS-1 to R-OBS-3. Existing tracing is kept. |
| New runtime subsections | New §6.16 Verification contracts, §6.17 Scenarios and test-state preparation, §6.18 Remote execution of QA runs and §6.19 Release qualification. The old §6.16 Sources is now §6.20. Nothing else referenced it. |
| §§6.13 and 7.19: acceptance | Each gets a "Scenarios and declared coverage" paragraph. No count or criterion changed. §7.19 keeps its numbering, so SEC-NF-29 is checked under §10 criterion 23. |
| §§6.15, 7.15 and R-ATT-* | §6.15 adds run records, later campaigns, the optional external framework, R-EVAL-1 and R-EVAL-2. §7.15 adds a cross-reference. §9 adds R-ATT-5. |
| §§7.8 and 7.9 | §7.8 adds "Release qualification is not a Goal's QA gate". §7.9 adds release evidence and keeps the witness rule. §7.2.4 and §7.20 add a threat row and R15. §7.17 adds SEC-F-48. §7.18 adds SEC-NF-29. |
| §§7.12 and 8 | §7.12 adds offline QA bundles on air-gapped targets. §8 adds "Release smoke". §9 adds R-PRV-7. |
| §9 | The requirement families are indexed, SEC-F range is now 1 to 48, and a story delivery map is added. |
| §§11 and 12 | Seven risk bullets. New open decisions Q27 to Q30. |
| §13 | Twelve tooling sources, with retrieval date 2026-10-12 and pinned commits where a repository exists. |
| (not in the handoff map) | New §4.8 Verification and QA tooling, in the "borrow and drop" form of the other §4 subsections. |

## 3. IDs

**Adopted from the handoff:** R-VER-1 to R-VER-3, R-QA-1, R-QA-2, R-RUN-1, R-RUN-2, R-REL-1 to R-REL-3, R-EVAL-1, R-EVAL-2. The wording is close to the handoff's. These IDs are provisional. The repository had no collisions at `45ae3f4`.

**Added in this change, not in the handoff:** R-OBS-1 to R-OBS-3, R-QA-3, R-RUN-3, R-RUN-4, R-REL-4, R-PRV-7, R-ATT-5, SEC-F-48, SEC-NF-29, security risk R15, §10 criteria 23 and 24, Q27 to Q30, and §4.8. Each turns a handoff statement that had no ID into a testable requirement, so the stories can map to it. The owner should review these first.

**Preserved:** every existing requirement, question, threat and SEC ID. A script comparison of the old and new file found none lost.

## 4. The handoff's open decisions (its section 11)

| Item | Status at `45ae3f4` | Action |
|---|---|---|
| Agent isolation | The conflict between lightweight in-process agents (§6.1) and Tier P process isolation (§7.3) is already reconciled. §6.1 says the Pi CLI runs in a hardened process, §6.8 supervises it, and §7.3.3 puts agent loops in Tier P. | No change to those sections. R-REL-3 requires the report to name the isolation configuration tested. A §11 risk bullet says a lower-isolation test build cannot certify the stronger profile. |
| Minimum topology | Open. | Q27, with the 4/3/3 proposal and a separate evaluator pool. |
| First QA authorization and Q5 | Open. SEC-F-21 requires a passed QA gate for unattended mode, and Q5 asks for the fixtures. | Q28 with a proposal. §7.8 bullet and SEC-F-48 forbid a universal QA flag and reuse of a release result as another Goal's QA gate. |
| Evaluation scope | §12 now has a proposed default for Q23 (versioned rubric, separate evaluator). | §6.15 keeps signed Attempt scoring. The catalog uses only objectively checkable Goals. Open-ended Goals follow Q23. |
| Controls not yet implemented | Nothing is implemented. | R-REL-1 and R-PRV-7 forbid mocks and replays for the live gate. R-REL-3 requires narrowed claims. |
| First environment and limits | Open. | Q29, with Q7 and Q8. |
| Release scope and exceptions | Not in the handoff list. | Q30. |

## 5. Differences between the handoff and the TRD

The handoff cites a 40-page PDF of TRD v1 with page numbers. The editable source is `docs/TRD.md`, which has moved on since that PDF: commit `45ae3f4` unified the Goal contract around intent and security posture. I checked every section reference in the handoff against the Markdown. All resolved. I did not carry the page numbers. I did not compare the Markdown with the PDF, because no PDF was available.

## 6. Repository audit (QA-00)

At `45ae3f4` the repository holds `README.md` and `docs/TRD.md`. There is no code, CLI, CI configuration, scenario, fixture or deployment script. The gap map is therefore the whole implementation. No command in the TRD is claimed to work.

## 7. Checks done and not done

**Done.** A script confirmed that every TRD section reference resolves (the four unresolved matches are existing references to sections inside the cited papers), that every requirement ID is defined, that tables and code fences are well formed, and that the §10 criteria are numbered 16 to 24 without a collision. The twelve handoff source URLs all resolve, and the pinned commit URLs resolve. For systemd I confirmed the exact statement that `RuntimeMaxSec=` has no effect on `Type=oneshot` services. For the pstack, Nextest, OpenSSH, Ansible and Inspect pages I matched the cited terms by keyword only.

**Not done.** No implementation and no command was run. The source pages were not read in full, so the handoff's descriptions of them (for example how Inspect separates datasets, execution and scoring) are not independently verified. The owner has not reviewed the proposals. The architecture visualization published earlier was built from commit `7f4077b`. It does not show this amendment or the Goal-posture change in `45ae3f4`.
