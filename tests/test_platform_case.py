"""Live-probe case generation emits the broker's fetch policy; the checker re-judges it.

No live inference and no network."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SCRIPTS = Path(__file__).parents[1] / "scripts"


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


platform = load("prepare_platform", "prepare_platform_case.py")
checker = load("check_run", "check_run.py")
remote = load("remote", "remote.py")


class PlatformCasePolicyTests(unittest.TestCase):
    def test_the_goal_carries_a_read_only_one_host_policy_by_default(self):
        goal, expected = platform.make_case(7)
        self.assertEqual(goal["fetch"], {
            "hosts": ["platform.solana.com"],
            "methods": ["GET", "HEAD", "OPTIONS"],
            "per_agent_rps": 1,
            "follow_redirects_off_list": False,
        })
        self.assertIn("fetch_url", goal["grants"])
        self.assertEqual(expected["coverage"]["fetch"], goal["fetch"])
        remote.validate_goal_limits(goal)

    def test_hosts_methods_and_rate_are_chosen_on_the_command_line_not_widened_silently(self):
        goal, _ = platform.make_case(
            7, "https://zelix-front-dev.fly.dev/", extra_hosts=["Zelix-Deck.vercel.app"],
            methods=["get", "head"], rps=0.5)
        self.assertEqual(goal["fetch"]["hosts"], ["zelix-front-dev.fly.dev", "zelix-deck.vercel.app"])
        self.assertEqual(goal["fetch"]["methods"], ["GET", "HEAD"])
        self.assertEqual(goal["fetch"]["per_agent_rps"], 0.5)
        # The RPC host and POST are listed only when asked for.
        goal, _ = platform.make_case(7, allow_rpc=True)
        self.assertEqual(goal["fetch"]["hosts"], ["platform.solana.com", "api.mainnet-beta.solana.com"])
        self.assertEqual(goal["fetch"]["methods"], ["GET", "HEAD", "OPTIONS", "POST"])

    def test_a_policy_the_broker_would_refuse_is_refused_here_first(self):
        for options in ({"methods": []}, {"methods": ["TRACE"]}, {"rps": 0}, {"rps": 5000}, {"rps": True}):
            with self.assertRaises(ValueError, msg=options):
                platform.make_case(7, **options)

    def test_write_case_freezes_the_policy_into_the_goal_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            goal_path = Path(directory) / "goal.json"
            expected = platform.write_case(
                7, goal_path, Path(directory) / "private" / "expected.json",
                "https://app.example.com/", extra_hosts=["api.example.com"])
            goal = json.loads(goal_path.read_text())
            self.assertEqual(goal["fetch"]["hosts"], ["app.example.com", "api.example.com"])
            self.assertEqual(expected["config_sha256"], checker.digest(goal_path))


def fetched(seq, agent, url, redirects=()):
    return {"seq": seq, "kind": "tool_completed", "data": {
        "agent_id": agent, "tool": "fetch_url",
        "args": {"url": url, "response": {"redirects": [{"status": 302, "to": to} for to in redirects]}},
        "result": {}}}


class FetchScopeAuditTests(unittest.TestCase):
    policy = {"hosts": ["zelix-deck.vercel.app", "*.fly.dev", "192.0.2.7"], "methods": ["GET"]}

    def issues(self, *events, policy=None):
        return checker.fetch_policy_issues(policy or self.policy, list(events))

    def test_listed_hosts_and_redirects_inside_the_list_are_clean(self):
        self.assertEqual(self.issues(
            fetched(1, "agent-01", "https://zelix-deck.vercel.app/a"),
            fetched(2, "agent-02", "https://ZELIX-front.fly.dev./b", ["https://x.fly.dev/c"]),
            fetched(3, "agent-03", "http://192.0.2.7:8080/"),
        ), [])

    def test_an_unlisted_url_or_redirect_hop_is_named_with_its_agent(self):
        issues = self.issues(
            fetched(1, "agent-01", "https://evil.example/a"),
            fetched(2, "agent-02", "https://zelix-deck.vercel.app/", ["https://accounts.google.com/x"]),
            fetched(3, "agent-03", "https://fly.dev/apex"),
            fetched(4, "agent-04", "https://zelix-deck.vercel.app@evil.example/"),
        )
        self.assertEqual(len(issues), 4, issues)
        self.assertIn("agent-01 fetched unlisted host 'evil.example'", issues[0])
        self.assertIn("accounts.google.com", issues[1])
        self.assertIn("'fly.dev'", issues[2])

    def test_other_events_are_ignored_and_following_redirects_off_the_list_is_the_policys_say(self):
        refused = {"seq": 9, "kind": "fetch_refused", "data": {"agent_id": "a", "url": "https://evil.example/"}}
        self.assertEqual(self.issues(refused), [])
        loose = {**self.policy, "follow_redirects_off_list": True}
        self.assertEqual(self.issues(
            fetched(1, "agent-01", "https://zelix-deck.vercel.app/", ["https://accounts.google.com/x"]),
            policy=loose), [])
        self.assertEqual(len(self.issues(fetched(2, "agent-01", "https://evil.example/"), policy=loose)), 1)


if __name__ == "__main__":
    unittest.main()
