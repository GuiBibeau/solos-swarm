# pstack in solos-swarm

pstack skills use Cursor names. In solos-swarm this file is part of your instructions, so you never need to open it. Map each Cursor name in a skill with the tables below. Wherever a skill says to read the Claude Code adapter (`../../docs/harness/claude-code.md`), use this file instead. If a Cursor field has no row, drop it and say so in your next report. Never pass a field or value the tool schema rejects.

You work one ticket. You report to the agent with `worker_report`, and the agent tells people. Your tools are the ones this session offers you. A row that names a tool you were not given does not apply to you, so skip its step with the line from "Not on this host".

If you run in Claude Code, its own tools are off. There is no `Read`, `Agent`, `TodoWrite`, `Skill` or slash command. Your tools are the agent's, named `mcp__swarm__<tool>`, so `read` is `mcp__swarm__read`. When this host gains subagents, a Claude Code session's subagents will run as Pi sessions, never through Claude Code's `Agent`.

## Tools

| Cursor | solos-swarm |
| --- | --- |
| `AskQuestion` | `worker_report` with status `waiting` and a `question`. Put the options in the question. It ends your turn, and the person's answer comes back as new input. Do not use `blocked` for a question, because it holds automatic input until a person writes. |
| Open a todolist | Keep the list in the text of a `worker_report` with status `progress`, and send a new one each time the list changes. Your latest report is what the operator sees in `agent status`. |
| Run another pstack skill (`/name`, "the **name** skill") | `read` `/pstack/skills/<name>/SKILL.md` and follow it. People here ask in chat, so never tell a person to type a slash command. Say what to ask for instead. |
| Read a file, open `playbooks/<title>.md`, "read the leaf skill" | `read` with an absolute path under `/pstack`, or on a ticket with a repository under `/work` and `/host`. Resolve a relative link against the directory of the file that holds it. A directory lists its entries. A long file comes in parts, so call again with `offset`. |
| `Grep`, `Glob`, a shell | `run_code`. Use `rg` if it is there, else `grep -rn` and `find`. |
| Edit or write a file | `run_code`, with its `files` argument or a heredoc. |
| A long command, a command in the background | `job_start`. `job_wait` waits for it and `job_stop` stops it. If it ends after your turn, its end comes back as new input. |
| `/loop`, a watcher, a wake | End your turn. Every wake is new input, such as a job's end or a person's message. For a timed wake, `job_start` with `sleep <seconds>`. Its end wakes you. |
| A web page, an API call, `curl`, `WebFetch` | `fetch_url`. It and the browser are your only network access. With `key`, it adds an API key that `vault_list` names, and you never see the key's value. |
| A new request on your ticket that is beyond its scope, or someone else's work | `worker_handback` with the message's trigger id. The Router judges it. Do not act on it yourself. |
| `create-skill` (Cursor's built-in) | Follow `/pstack/skills/poteto-mode/playbooks/authoring-a-skill.md`. |
| Any MCP tool | The agent's tools are your only MCP tools. |

## Models

| Cursor | solos-swarm |
| --- | --- |
| The `pstack-models.mdc` rule, `~/.cursor/rules/`, `~/.claude/rules/` | The "Your pstack-models rule" section of your instructions. Do not look for a file. |
| `claude-<family>-<version>-<effort>[-fast]`, a `grok-*`, `composer-*`, `gpt-*` or other slug, `auto`, `inherit-parent` | Your own model. Every role on this host runs on it. |
| An effort level, `-fast`, "raise it one step" | Drop them. Your model runs at one level. |
| A model family, "never from the parent's family", a judge or verifier "on a different model family" | This host runs one model family. So, as this host's own adaptation and not a pstack rule, a role that needs another family runs on your model with a fresh look. Start it from its brief alone, as if you had not seen the work. Say in your report that one family played both roles. |

## Paths

| Cursor | solos-swarm |
| --- | --- |
| `<pstack>`, `../<name>/SKILL.md`, `pstack/skills/...`, a skill's `references/` or `scripts/` | `/pstack/...`, read-only. `read` it. Run pstack's bash scripts with `run_code` only when your instructions say that `run_code` has `/pstack`. |
| `https://github.com/cursor/plugins/blob/main/pstack/<path>` | `/pstack/<path>` when that file exists. Give people the URL when they need a link. |
| The work dir, scratch files, the agent store's `docs/`, a Project tracker `notes.md`, `decisions.tsv` | `/work` in `run_code`, this ticket's own directory. It keeps what you write between calls and for any later work on this ticket. On a ticket with a repository, `/work` is the clone, so list your notes in `.git/info/exclude` and keep them out of your commits. |
| A repository: the work dir as a clone, `git log`, `git blame`, a worktree off main | On a ticket with a repository, `/work` is a clone of it at its base's tip, with its history and your commit identity. Run git there with `run_code`, and make your own branch with `git switch -c <name>`. `/host/repo.json` names the repository, its base, the commit the clone started from and the operator's notes. A ticket with no repository has none, so skip with the line. |
| `.cursor/skills/<name>/` in a project | `/work/.agents/skills/<name>/` on a ticket with a repository. `read` it. |
| `~/.cursor/...`, `~/.claude/...`, `.cursor/rules/*.mdc`, `CLAUDE.md`, `AGENTS.md` | None of these exist here. |

## Verification

| Cursor | solos-swarm |
| --- | --- |
| "Verify on the matching surface", **prove-it-works**, a test run you can show | Run the real thing with `run_code` or a job, and keep its output in `/work` so your report can quote it. |
| `control-cli` | `run_code`. |
| `control-ui` | `browser_open`, `browser_act`, `browser_read` and `browser_login`, for an http(s) site the browser can reach. Skip a desktop or native app with the line. |
| **benchmark-checklist**, a number you measured | `run_code` or a job. `run_code` caps each process at 300 s of CPU and has no network. Name both limits next to any number you report. |

## Not on this host

When a step needs one of these, put one line in your next report, `Skipped in solos-swarm: <step>. <reason>.`, then go on. Do not stop, ask or retry. When a later step needs what the skipped step made, leave a placeholder and name it in the line. A row marked "Later" comes to this host in a later change.

| Cursor | solos-swarm |
| --- | --- |
| `Task`, a subagent, `poteto-agent`, `subagent_type`, `readonly`, `environment`, `resume`, `file_attachments`, spawning N "in a single message" | Later. This host has no subagents yet, so pstack's rule for that case applies: "A subagent forbidden to spawn satisfies this by owning the diff directly." Do the subagent's work yourself in this session. Where a step wants several independent runs, such as explorers, arena runners or reviewers, do them one after another, each from its brief alone, and say so in your report. |
| A pull request: `git push`, `gh pr create`, `gh pr view`, `gh pr checks`, `gh pr merge`, `watch-pr`, Origin, **Opening a PR**, **Babysit**, **Shipping** | Later. This host opens no pull requests yet. Skip with the line. Your result is your `ready` report. |
| An independent verifier per pull request, a verdict at a head commit | Later. Skip with the line, and verify with `run_code` as Verification says. |
| **create-verification-skill** | Later. The skill it writes stays in `/work` until this host opens pull requests. Skip with the line. |
| Transcripts in `agent-transcripts/`, as **reflect**, **recall** and **session-pickup** read them | Later. No transcript is readable here. Skip with the line. |
| Cursor Automations, routine wakes, `update_state`, `SendToUser` | Skip with the line. Schedules are the operator's to configure. |
| Bugbot, Cursor cloud agents | Skip with the line. |
| `deslop` from `cursor-team-kit` | Skip with the line. Run `/pstack/skills/no-comments/SKILL.md` and **unslop** as written. |
| `scripts/check-plan.mjs`, `watch-pr`, anything that needs `node`, `bun`, `gh` or the network in `run_code` | Skip with the line, unless `command -v` finds the tool in `run_code`. |
| **setup-pstack** | Skip with the line. The operator sets models in agent.yaml. |
| MCP discovery in **why**, its evidence categories | The agent's tools are your only MCP tools. Record each category that has no tool as a gap, "No matching MCP available", as **why** says. |
| Irreversible actions in **Autonomy** (force-push to shared branches, deploys, data deletion, customer messages) | There is no tool for any of them here. You never message people yourself. |
