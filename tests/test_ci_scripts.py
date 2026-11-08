"""The two scripts CI runs to decide what to run: scripts/ci-changes.sh (which jobs a change needs) and
scripts/has-doc-tests.sh (whether rustdoc has anything to run). A wrong answer from the first skips tests, so the cases
here are the ones where skipping would be a mistake."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

root = Path(__file__).parents[1]
CHANGES = root / "scripts/ci-changes.sh"
DOCTESTS = root / "scripts/has-doc-tests.sh"


def flags(files, **env):
    """What ci-changes.sh says for these changed files, as {"rust": bool, "web": bool}."""
    out = subprocess.run(
        ["bash", str(CHANGES), "--stdin"], input="".join(f + "\n" for f in files),
        capture_output=True, text=True, check=True, env={**os.environ, **env},
    ).stdout
    return {k: v == "true" for k, v in (line.split("=") for line in out.split())}


def git(cwd, *args):
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", *args],
        cwd=cwd, check=True, capture_output=True,
    )


class CiChanges(unittest.TestCase):
    def test_a_pull_request_that_only_edits_documentation_runs_no_job_that_builds(self):
        self.assertEqual(flags(["docs/agent-mode/production.md", "README.md", "CHANGELOG.md"]), {"rust": False, "web": False})
        self.assertEqual(flags([".github/ISSUE_TEMPLATE/bug.yml", ".github/pull_request_template.md", "LICENSE"]),
                         {"rust": False, "web": False})

    def test_documentation_that_the_binary_or_a_test_reads_is_code(self):
        # docs/harness/solos-swarm.md is include_str!'d into the binary; a test reads the GitHub App manifest.
        self.assertTrue(flags(["docs/harness/solos-swarm.md"])["rust"])
        self.assertTrue(flags(["docs/agent-mode/github-app/manifest.json"])["rust"])
        self.assertTrue(flags(["docs/agent-mode/some-diagram.png"])["rust"])

    def test_markdown_anywhere_else_is_not_assumed_to_be_documentation(self):
        # vendor/pstack/skills/*.md is read by tests; only the README-style files at the root and docs/ are skipped.
        self.assertTrue(flags(["vendor/pstack/skills/poteto-mode/SKILL.md"])["rust"])
        self.assertTrue(flags(["notes/swarm-arch-notes.md"])["rust"])
        self.assertTrue(flags(["skills/verify-solos-swarm/SKILL.md"])["rust"])

    def test_a_dashboard_only_change_skips_rust_and_a_rust_only_change_skips_the_dashboard(self):
        self.assertEqual(flags(["web/agent-dashboard/app/page.tsx", "web/agent-dashboard/package-lock.json"]),
                         {"rust": False, "web": True})
        self.assertEqual(flags(["src/agent/health.rs", "Cargo.lock"]), {"rust": True, "web": False})
        self.assertEqual(flags(["web/agent-dashboard/app/page.tsx", "docs/agent-mode/production.md"]),
                         {"rust": False, "web": True})

    def test_a_mixed_change_runs_both(self):
        self.assertEqual(flags(["web/agent-dashboard/app/page.tsx", "src/lib.rs"]), {"rust": True, "web": True})

    def test_a_change_to_the_workflow_or_to_what_decides_runs_everything(self):
        for changed in (".github/workflows/ci.yml", "scripts/ci-changes.sh", "scripts/has-doc-tests.sh"):
            self.assertEqual(flags([changed]), {"rust": True, "web": True}, changed)

    def test_everything_else_is_code(self):
        for changed in ("scripts/agent-deploy", "scripts/ship", "tests/agent_deploy.rs", ".config/nextest.toml",
                        "package-lock.json", "extensions/broker.ts", "assets/tokenizers/qwen3/tokenizer.json",
                        "Cargo.toml", "src/main.rs"):
            self.assertTrue(flags([changed])["rust"], changed)

    def test_an_empty_diff_or_an_event_that_is_not_a_pull_request_runs_everything(self):
        self.assertEqual(flags([]), {"rust": True, "web": True})
        with tempfile.TemporaryDirectory() as empty:
            for event in ("push", "workflow_dispatch", ""):
                out = subprocess.run(["bash", str(CHANGES)], cwd=empty, capture_output=True, text=True,
                                     env={**os.environ, "GITHUB_EVENT_NAME": event}).stdout
                self.assertEqual(out.split(), ["rust=true", "web=true"], event)

    def test_a_pull_request_is_what_its_merge_commit_adds_to_the_base(self):
        # CI checks out refs/pull/N/merge: a merge commit whose first parent is the base branch.
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            git(repo, "init", "-q", "-b", "main")
            (repo / "src").mkdir()
            (repo / "docs").mkdir()
            (repo / "src/lib.rs").write_text("// one\n")
            (repo / "docs/guide.md").write_text("one\n")
            git(repo, "add", ".")
            git(repo, "commit", "-qm", "base")

            def pull_request(branch, path, text):
                git(repo, "checkout", "-q", "-b", branch, "main")
                (repo / path).write_text(text)
                git(repo, "commit", "-qam", branch)
                git(repo, "checkout", "-q", "main")
                # Main moves on, with code: that is not part of the pull request.
                (repo / "src/other.rs").write_text(branch + "\n")
                git(repo, "add", ".")
                git(repo, "commit", "-qm", "main moved " + branch)
                git(repo, "checkout", "-q", "--detach", "main")
                git(repo, "merge", "-q", "--no-ff", "-m", "merge " + branch, branch)

            def says():
                out = subprocess.run(["bash", str(CHANGES)], cwd=repo, capture_output=True, text=True, check=True,
                                     env={**os.environ, "GITHUB_EVENT_NAME": "pull_request"}).stdout
                return out.split()

            pull_request("docs-only", "docs/guide.md", "two\n")
            self.assertEqual(says(), ["rust=false", "web=false"])
            git(repo, "checkout", "-q", "main")
            pull_request("code", "src/lib.rs", "// two\n")
            self.assertEqual(says(), ["rust=true", "web=false"])


def doc_tests_in(**files):
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "src"
        src.mkdir()
        for name, text in files.items():
            (src / f"{name}.rs").write_text(text)
        return subprocess.run(["bash", str(DOCTESTS), str(src)], capture_output=True).returncode == 0


class HasDocTests(unittest.TestCase):
    def test_an_unmarked_or_rust_fence_is_a_doc_test(self):
        self.assertTrue(doc_tests_in(a="/// ```\n/// assert!(true);\n/// ```\nfn f() {}\n"))
        self.assertTrue(doc_tests_in(a="//! ```rust,no_run\n//! f();\n//! ```\n"))
        self.assertTrue(doc_tests_in(a="/// ```should_panic edition2021\n/// panic!();\n/// ```\nfn f() {}\n"))

    def test_a_fence_marked_with_another_language_is_not(self):
        self.assertFalse(doc_tests_in(a="//! ```text\n//! not code\n//! ```\n"))
        self.assertFalse(doc_tests_in(a="/// ```json\n/// {}\n/// ```\nfn f() {}\n"))
        self.assertFalse(doc_tests_in(a="/// ```sh\n/// ls\n/// ```\nfn f() {}\n"))

    def test_the_closing_fence_of_a_text_block_is_not_mistaken_for_a_doc_test(self):
        self.assertFalse(doc_tests_in(a="//! ```text\n//! a\n//! b\n//! ```\n//!\n//! ```text\n//! c\n//! ```\n"))

    def test_text_that_starts_with_backticks_and_has_more_in_it_is_not_a_fence(self):
        # config.rs has "/// ```json fence whose object says `ok: true`": CommonMark reads that as a paragraph.
        text = "/// ```json fence whose object says `ok: true`.\n/// ```text\n/// x\n/// ```\nfn f() {}\n"
        self.assertFalse(doc_tests_in(a=text))

    def test_a_doc_test_in_a_later_file_is_found_and_plain_comments_are_not_doc_tests(self):
        self.assertTrue(doc_tests_in(a="//! ```text\n//! x\n//! ```\n", b="/// ```\n/// 1;\n/// ```\nfn f() {}\n"))
        self.assertFalse(doc_tests_in(a="// ```\n// not a doc comment\n// ```\nfn f() {}\n"))


if __name__ == "__main__":
    unittest.main()
