"""Exercise the built wheel from a clean environment outside the checkout."""

from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import venv


def main():
    wheels = sorted(Path("dist").glob("*.whl"))
    if len(wheels) != 1:
        raise SystemExit("Build into a clean dist directory containing exactly one wheel.")
    wheel = wheels[0].resolve(strict=True)
    clean_env = os.environ.copy()
    for name in ("PYTHONPATH", "PYTHONHOME", "PYTHONUSERBASE"):
        clean_env.pop(name, None)
    clean_env["PYTHONNOUSERSITE"] = "1"
    with tempfile.TemporaryDirectory(prefix="demon-installed-") as temp:
        root = Path(temp).resolve()
        environment = root / "venv"
        venv.EnvBuilder(with_pip=True, system_site_packages=False).create(environment)
        scripts = environment / ("Scripts" if os.name == "nt" else "bin")
        interpreter = scripts / ("python.exe" if os.name == "nt" else "python")

        def run(args, *, shell=False):
            result = subprocess.run(args, shell=shell, cwd=root, env=clean_env,
                                    text=True, capture_output=True, timeout=30)
            if result.returncode:
                raise AssertionError(f"installed command failed: {result.stderr}")
            return result.stdout

        run([str(interpreter), "-m", "pip", "install", "--isolated", "--no-index", "--no-deps", str(wheel)])
        suffix = ".exe" if os.name == "nt" else ""
        demon = str(scripts / ("demon" + suffix))
        for command in ("demon", "demon-mode"):
            run([str(scripts / (command + suffix)), "--help"])
            assert "0.1.1" in run([str(scripts / (command + suffix)), "--version"])
        now = datetime.now(timezone.utc)
        # Include shell-sensitive characters to prove argument preservation.
        db = root / "ledger's $literal & percent%.sqlite3"
        prefix = [demon, "--db", str(db)]
        policy = json.loads(run(prefix + ["record", "--provider", "codex", "--weekly", "18",
            "--resets", (now + timedelta(hours=100)).isoformat(), "--five", "12",
            "--five-resets", (now + timedelta(hours=4)).isoformat(), "--at", now.isoformat(), "--json"]))
        assert policy["weekly_used_percent"] == 18
        assert json.loads(run(prefix + ["history", "--provider", "codex", "--json"]))[0]["short_used_percent"] == 12
        assert "Mode:" in run(prefix + ["context", "--provider", "codex"])
        assert json.loads(run(prefix + ["doctor", "--provider", "codex", "--json"]))["source"] == "manual"
        for client in ("codex", "claude"):
            if client == "claude":
                # Give each client its own real synthetic provider reading.
                run(prefix + ["record", "--provider", client, "--weekly", "18", "--resets",
                    (now + timedelta(hours=100)).isoformat(), "--at", now.isoformat()])
            config = json.loads(run(prefix + ["hook-config", "--client", client, "--format", "native"]))
            handler = config["hooks"]["UserPromptSubmit"][0]["hooks"][0]
            if "args" in handler:
                output = run([handler["command"], *handler["args"]])
            else:
                output = run(handler["command"], shell=True)
            assert len(output.encode("utf-8")) <= 2048
            assert "18.0% measured" in json.loads(output)["hookSpecificOutput"]["additionalContext"]
        # Read current documented app-server fields through the installed package.
        export = root / "limits.json"
        export.write_text(json.dumps({"id": 6, "result": {"rateLimits": {
            "limitId": "codex", "secondary": {"usedPercent": 19, "windowDurationMins": 10080,
                "resetsAt": (now + timedelta(hours=100)).timestamp()},
        }}}), encoding="utf-8-sig")
        imported = json.loads(run(prefix + ["ingest", "codex-app-server", "--file", str(export),
            "--at", (now + timedelta(milliseconds=1)).isoformat(), "--json"]))
        assert imported["source"] == "codex_app_server"
        assert imported["weekly_used_percent"] == 19
        print("Installed wheel: both entry points, ledger, status, context, doctor, native hook commands, and OpenAI export import passed.")


if __name__ == "__main__":
    main()
