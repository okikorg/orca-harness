"""Exercise the release size gate used by CI with exact byte boundaries."""

import pathlib
import subprocess
import tempfile
import unittest


GATE = pathlib.Path(__file__).resolve().parents[2] / "ci/check-binary-size.sh"


class BinarySizeTests(unittest.TestCase):
    def test_strict_decimal_mb_limit(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = pathlib.Path(tmp) / "orcacode"
            for size, expected in [(6_699_999, 0), (6_700_000, 1), (6_700_001, 1)]:
                with self.subTest(size=size):
                    with binary.open("wb") as handle:
                        handle.truncate(size)
                    result = subprocess.run(
                        ["bash", str(GATE), str(binary)], capture_output=True, text=True
                    )
                    self.assertEqual(expected, result.returncode, result.stdout + result.stderr)

    def test_missing_binary_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                ["bash", str(GATE), str(pathlib.Path(tmp) / "missing")],
                capture_output=True, text=True,
            )
            self.assertEqual(1, result.returncode)
            self.assertIn("missing release binary", result.stderr)


if __name__ == "__main__":
    unittest.main()
