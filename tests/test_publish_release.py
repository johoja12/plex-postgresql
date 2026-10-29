"""Release dispatch must validate and target the same maintained repository."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/publish-release.sh"


class PublishReleaseTests(unittest.TestCase):
    def run_script(self, repo=None, missing=False, dispatch_fails=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "calls"
            for command in ("git", "gh"):
                mock = root / command
                mock.write_text(
                    "#!/usr/bin/env python3\n"
                    "import json, os, sys\n"
                    "from pathlib import Path\n"
                    "with open(os.environ['CALL_LOG'], 'a') as log:\n"
                    "    log.write(json.dumps([Path(sys.argv[0]).name] + sys.argv[1:]) + '\\n')\n"
                    "sys.exit(int(os.environ.get('FAIL_' + Path(sys.argv[0]).name.upper(), '0')))\n"
                )
                mock.chmod(0o755)
            env = os.environ.copy()
            env.pop("PLEX_PG_RELEASE_REPO", None)
            env.update(PATH=f"{root}:{env['PATH']}", CALL_LOG=str(log),
                       FAIL_GIT=str(int(missing)), FAIL_GH=str(int(dispatch_fails)))
            if repo:
                env["PLEX_PG_RELEASE_REPO"] = repo
            result = subprocess.run(["sh", str(SCRIPT), "v-test"], env=env,
                                    capture_output=True, text=True)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            return result, calls

    def test_default_dispatches_to_maintained_fork(self):
        result, calls = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0], ["git", "ls-remote", "--exit-code", "--tags",
                                   "https://github.com/johoja12/plex-postgresql.git", "refs/tags/v-test"])
        self.assertEqual(calls[1], ["gh", "workflow", "run", "Release Linux Artifacts",
                                   "--repo", "johoja12/plex-postgresql", "-f", "tag=v-test"])

    def test_override_validates_tag_on_selected_repository(self):
        result, calls = self.run_script(repo="example/other")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("https://github.com/example/other.git", calls[0])
        self.assertIn("example/other", calls[1])

    def test_missing_tag_does_not_dispatch(self):
        result, calls = self.run_script(missing=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls), 1)
        self.assertIn("is not on johoja12/plex-postgresql", result.stderr)

    def test_failed_dispatch_does_not_report_success(self):
        result, calls = self.run_script(dispatch_fails=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls), 2)
        self.assertNotIn("dispatched", result.stdout)


if __name__ == "__main__":
    unittest.main()
