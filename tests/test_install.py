import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
WINDOWS = os.name == "nt"


def copy_repo(destination: Path) -> Path:
    target = destination / "repo"
    shutil.copytree(
        ROOT, target,
        ignore=shutil.ignore_patterns(".git", ".venv", "build", "dist", "__pycache__", "*.egg-info"),
    )
    return target


class InstallTests(unittest.TestCase):
    def test_install_cmd_is_crlf(self):
        data = (ROOT / "install.cmd").read_bytes()
        self.assertEqual(data.count(b"\n"), data.count(b"\r\n"))

    def test_install_sh_is_lf_and_parses(self):
        self.assertNotIn(b"\r", (ROOT / "install.sh").read_bytes())
        bash = shutil.which("bash")
        if bash is None:
            self.skipTest("bash is not on PATH")
        result = subprocess.run([bash, "-n", "install.sh"], cwd=ROOT, capture_output=True, text=True)
        diagnostic = (result.stdout + result.stderr).lower()
        empty_wsl = "distribution" in diagnostic or (
            "wsl" in diagnostic and "execvpe(/bin/bash) failed" in diagnostic and
            "no such file or directory" in diagnostic
        )
        if result.returncode != 0 and empty_wsl:
            self.skipTest("bash on PATH is a WSL launcher with no usable Linux shell")
        self.assertEqual(result.returncode, 0, result.stderr)

    @unittest.skipUnless(WINDOWS, "install.cmd is Windows only")
    def test_install_cmd_dry_run_changes_nothing(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = copy_repo(Path(temp))
            result = subprocess.run(
                ["cmd", "/c", str(repo / "install.cmd"), "/dry-run"], cwd=repo, stdin=subprocess.DEVNULL,
                capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn(".venv", result.stdout)
            self.assertIn("shortcut", result.stdout.lower())
            self.assertFalse((repo / ".venv").exists())

    @unittest.skipUnless(WINDOWS and os.environ.get("DEMON_TEST_REAL_INSTALL"), "set DEMON_TEST_REAL_INSTALL=1 to run a real install (uses pip)")
    def test_install_cmd_real_run(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = copy_repo(Path(temp))
            shortcuts = Path(temp) / "shortcuts"
            env = {**os.environ, "INSTALL_SHORTCUT_DIR": str(shortcuts), "INSTALL_NO_PAUSE": "1"}
            result = subprocess.run(
                ["cmd", "/c", str(repo / "install.cmd")], cwd=repo, env=env, stdin=subprocess.DEVNULL,
                capture_output=True, text=True, timeout=240,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertTrue((repo / ".venv" / "Scripts" / "pythonw.exe").is_file())
            self.assertEqual(sorted(p.name for p in shortcuts.glob("*.lnk")), ["Demon Mode (Start menu).lnk", "Demon Mode.lnk"])
            self.assertIn("click the Demon Mode shortcut", result.stdout)
            smoke = subprocess.run(
                [str(repo / ".venv" / "Scripts" / "python.exe"), "-c",
                 "from importlib import resources; import demon_mode; "
                 "print((resources.files('demon_mode') / 'web' / 'app.js').is_file(), "
                 "(resources.files('demon_mode') / 'brand' / 'mark.svg').is_file())"],
                cwd=temp, capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(smoke.stdout.strip(), "True True", smoke.stderr)


if __name__ == "__main__":
    unittest.main()
