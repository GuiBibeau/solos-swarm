"""dashboard-tunnel.sh argument handling, offline (--dry-run never calls ssh)."""
import subprocess
import unittest
from pathlib import Path

SCRIPT = str(Path(__file__).parents[1] / "scripts/dashboard-tunnel.sh")


def run(*a):
    return subprocess.run(["bash", SCRIPT, *a], capture_output=True, text=True)


class Tunnel(unittest.TestCase):
    def test_default_command_binds_loopback_on_both_ends(self):
        r = run("--dry-run")
        self.assertEqual(r.returncode, 0)
        self.assertIn("ssh -N -L 4173:127.0.0.1:4173 -o ExitOnForwardFailure=yes remote-ai", r.stdout)
        self.assertIn("open http://127.0.0.1:4173/", r.stdout)

    def test_start_port_and_host(self):
        r = run("--dry-run", "--start", "run-1", "--port", "4999", "--host", "box")
        self.assertIn("ssh box systemctl --user start solos-dashboard@run-1", r.stdout)
        self.assertIn("-L 4999:127.0.0.1:4999", r.stdout)

    def test_rejects_bad_input(self):
        for bad in (["--port", "80"], ["--port", "x"], ["--host", "a b"], ["--start", "a;b"], ["--bogus"]):
            self.assertEqual(run("--dry-run", *bad).returncode, 2, bad)


if __name__ == "__main__":
    unittest.main()
