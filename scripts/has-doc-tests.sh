#!/usr/bin/env bash
# Succeeds when a doc comment (`///` or `//!`) under the given directories (default: src crates) holds a code block
# that rustdoc would run as a test: an unmarked fence, or one marked only with rustdoc's own words (rust, ignore,
# no_run, should_panic, compile_fail, test_harness, edition2021). A fence with any other word, such as ```text or
# ```json, is not Rust to rustdoc. CI runs the doc tests only when this succeeds: running rustdoc over a crate with
# none costs five seconds on the path every test waits for.
dirs=("$@")
((${#dirs[@]})) || dirs=(src crates)
find "${dirs[@]}" -name '*.rs' -print0 | xargs -0 awk '
	FNR == 1 { open = 0 }
	/^[ \t]*\/\/[\/!][ \t]*```/ {
		info = $0
		sub(/^[ \t]*\/\/[\/!][ \t]*```/, "", info)
		if (index(info, "`") > 0) next # CommonMark: a backtick in the info string means this is text, not a fence
		if (open) { open = 0; next }
		open = 1
		gsub(/[ \t\r]+/, ",", info)
		n = split(info, tags, ",")
		ok = 1
		for (i = 1; i <= n; i++)
			if (tags[i] != "" && tags[i] !~ /^(rust|ignore.*|no_run|should_panic|compile_fail|test_harness|edition[0-9]+)$/) ok = 0
		if (ok) { print FILENAME; found = 1 }
	}
' | grep -q .
