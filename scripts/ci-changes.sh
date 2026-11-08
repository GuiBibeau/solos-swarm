#!/usr/bin/env bash
# Which parts of CI does a change need? Prints `rust=true|false` and `web=true|false`, for $GITHUB_OUTPUT.
#
#   scripts/ci-changes.sh           in CI: what a pull request changes, read from the merge commit that was checked out
#                                   (its first parent is the base branch)
#   scripts/ci-changes.sh --stdin   the changed file names, one per line, on stdin (the tests use this)
#
# A pull request that touches only documentation, or only the dashboard (web/agent-dashboard), skips the Rust jobs.
# One that touches no dashboard file skips the web job. Everything else runs everything, and so does anything this
# cannot tell: a push to main, a manual run, a diff it cannot read, an empty one.
#
# Documentation here is Markdown outside docs/harness/ (the adapter in docs/harness/ is compiled into the binary),
# the licence files and the issue and pull request templates. A test reads docs/agent-mode/github-app/manifest.json,
# so a file in docs/ that is not Markdown counts as code.
set -u

all() { echo "rust=true"; echo "web=true"; exit 0; }

if [[ ${1:-} == --stdin ]]; then
	files=$(cat)
elif [[ ${GITHUB_EVENT_NAME:-} == pull_request ]]; then
	files=$(git diff --name-only HEAD^1 HEAD 2> /dev/null) || all
else
	all
fi
[[ -n $files ]] || all

rust=false
web=false
while IFS= read -r f; do
	[[ -n $f ]] || continue
	if [[ $f == web/agent-dashboard/* ]]; then
		web=true
	elif [[ $f == .github/workflows/ci.yml || $f == scripts/ci-changes.sh || $f == scripts/has-doc-tests.sh ]]; then
		rust=true
		web=true
	elif [[ $f == docs/harness/* ]]; then
		rust=true
	elif [[ $f == docs/*.md || ($f == *.md && $f != */*) || $f == LICENSE || $f == NOTICE ||
		$f == .github/ISSUE_TEMPLATE/* || $f == .github/pull_request_template.md ]]; then
		:
	else
		rust=true
	fi
done <<< "$files"
echo "rust=$rust"
echo "web=$web"
