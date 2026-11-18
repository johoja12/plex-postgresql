"""The upgrade comparison must report calls from phases missing on either side."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class UpgradeRigCompareTests(unittest.TestCase):
    def test_reports_phases_present_on_only_one_side(self):
        script = Path(__file__).resolve().parents[1] / "scripts/upgrade-rig/cmp.py"
        for side in ("A", "B"):
            with self.subTest(side=side), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for name in ("A", "B"):
                    (root / name).mkdir()
                phase = root / side / "voice"
                phase.mkdir()
                (phase / "0001.json").write_text(json.dumps({
                    "method": "GET", "label": "voice ABCDEF",
                    "status": 200, "path": "/hubs/search/voice",
                    "json": {}, "text": None,
                }))
                result = subprocess.run(
                    [sys.executable, str(script), str(root / "A"), str(root / "B")],
                    check=True, capture_output=True, text=True,
                )
                counts = "1 calls only in A" if side == "A" else "1 only in B"
                self.assertIn(counts, result.stdout)
                self.assertIn("voice:", result.stdout)
