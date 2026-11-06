"""Differential parity check: identical randomized operations through Python 0.1.1 and this crate.

Developer-only tooling; the product does not use it. Every output line and the resulting SQLite
rows must match byte for byte, including a hand-off where Python writes the first half of a ledger
and Rust continues on the same file. Usage, from the repository root:

    git worktree add ../demon-py 69c93dd          # the last Python commit
    cargo build --manifest-path tools/parity/Cargo.toml
    DEMON_PY_SRC=../demon-py python3 tools/parity/compare.py 0:500 1000:1040 2000:2060

Seeds below 1000 are mixed timelines, 1000-1999 dense minute-level histories that exercise
retention and the pace upper bound, 2000 and up edited or legacy ledgers (uncertain history).
"""
import json, os, random, shutil, sqlite3, subprocess, sys, tempfile
from datetime import datetime, timedelta, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
RUST = os.environ.get("DEMON_PARITY_BIN", os.path.join(HERE, "target", "debug", "demon-parity"))
PY_HARNESS = os.path.join(HERE, "py_harness.py")
UTC = timezone.utc

def iso(at, r):
    text = at.isoformat()
    if r.random() < 0.6:
        text = text.replace("+00:00", "Z")
    return text

def stamp(at, r):
    if r.random() < 0.08:
        return round(at.timestamp(), r.choice([0, 3, 6]))
    if r.random() < 0.05:  # a non-UTC offset spelling of the same instant
        return at.astimezone(timezone(timedelta(hours=-5))).isoformat()
    return iso(at, r)

def gen(seed):
    r = random.Random(seed)
    now = datetime(2026, 10, 1, tzinfo=UTC) + timedelta(minutes=r.randrange(0, 60 * 24 * 20), microseconds=r.randrange(10**6))
    ops = []
    state = {}
    dense = r.random() < 0.4
    count = r.choice([6, 30, 90, 160])
    providers = ["claude", "codex"] if r.random() < 0.3 else [r.choice(["claude", "codex"])]
    for p in providers:
        state[p] = dict(reset=now + timedelta(hours=r.uniform(2, 168)), used=r.uniform(0, 70),
                        short_reset=now + timedelta(hours=r.uniform(0.2, 5)), short=r.uniform(0, 90))
    last = None
    for _ in range(count):
        now += timedelta(minutes=r.choice([1, 2, 5, 10]) if dense else r.choice([1, 10, 30, 60, 90, 200, 420]),
                         microseconds=r.randrange(10**6) if r.random() < 0.5 else 0)
        p = r.choice(providers)
        s = state[p]
        if now >= s["reset"]:
            s["reset"] = now + timedelta(hours=168) - timedelta(minutes=r.randrange(0, 180)); s["used"] = r.uniform(0, 5)
        if now >= s["short_reset"]:
            s["short_reset"] = now + timedelta(hours=5) - timedelta(minutes=r.randrange(0, 40)); s["short"] = r.uniform(0, 10)
        s["used"] = max(0, min(100, s["used"] + r.uniform(-1.0 if r.random() < 0.06 else 0, 2.5)))
        s["short"] = max(0, min(100, s["short"] + r.uniform(-2 if r.random() < 0.06 else 0, 12)))
        observed = now - timedelta(minutes=r.choice([0, 0, 0, 0, 0, 1, 45, 90, 200, 400]))
        roll = r.random()
        if roll < 0.04:
            observed = now + timedelta(seconds=r.choice([30, 59, 61, 120]))
        elif roll < 0.08:
            observed = now - timedelta(hours=r.uniform(0.2, 30))
        reset = s["reset"] + timedelta(seconds=r.choice([0, 0, 0, 1, 2, 3]) if r.random() < 0.3 else 0)
        op = {"op": "record", "db": "main", "provider": p, "observed_at": stamp(observed, r),
              "weekly": round(s["used"], r.choice([0, 1, 3, 9])), "reset": stamp(reset, r),
              "short": None, "short_reset": None, "now": iso(now, r)}
        if r.random() < 0.8:
            op["short"] = round(s["short"], r.choice([0, 1, 3]))
            op["short_reset"] = stamp(s["short_reset"], r)
        if r.random() < 0.01:
            op["weekly"] = r.choice([101, -1, True, "12"])
        if r.random() < 0.01:
            op["reset"] = stamp(observed - timedelta(minutes=1), r)
        if last is not None and r.random() < 0.05:
            op = dict(last, now=iso(now, r))  # exact replay
        elif last is not None and r.random() < 0.04:
            bumped = last["weekly"] + 0.5 if isinstance(last["weekly"], (int, float)) and not isinstance(last["weekly"], bool) else last["weekly"]
            op = dict(last, weekly=bumped, now=iso(now, r))
        ops.append(op)
        last = op
        for _ in range(r.choice([0, 0, 1, 2])):
            later = now + timedelta(minutes=r.choice([0, 1, 30, 70, 200, 400, 2000]))
            kind = r.choice(["status", "status", "context", "hook", "history"])
            ops.append({"op": kind, "db": "main", "provider": r.choice(providers), "now": iso(later, r),
                        "limit": r.choice([1, 20, 1000])})
    for p in providers:
        ops.append({"op": "status", "db": "main", "provider": p, "now": iso(now, r)})
    return ops

def gen_dense(seed):
    r = random.Random(seed)
    start = datetime(2026, 10, 3, tzinfo=UTC) + timedelta(minutes=r.randrange(0, 600))
    week_reset = start + timedelta(hours=r.uniform(120, 166))
    rate = r.uniform(0.002, 0.03)
    ops = []
    timeline = [0] + list(range(60, 120)) + list(range(120, 24 * 60 + 1, 60)) + list(range(24 * 60 + 20, 25 * 60 + 31))
    timeline += list(range(25 * 60 + 31, 25 * 60 + 31 + r.randrange(10, 120)))
    if r.random() < 0.5:
        timeline += [timeline[-1] + h * 60 for h in range(1, r.randrange(2, 30))]
    for minute in timeline:
        at = start + timedelta(minutes=minute)
        reset = week_reset + timedelta(seconds=[2, 0, 1][0 if minute == 0 else (1 if minute == 120 else 2)])
        short_reset = start + timedelta(minutes=(minute // 300 + 1) * 300)
        op = {"op": "record", "db": "main", "provider": "claude", "observed_at": iso(at, r), "weekly": 3.2 + minute * rate,
              "reset": iso(reset, r), "short": 30.0 + (minute % 300) * 0.02, "short_reset": iso(short_reset, r), "now": iso(at, r)}
        ops.append(op)
        if minute == 240 or r.random() < 0.01:
            ops.append(dict(op, weekly=op["weekly"] + 0.2, short=op["short"] + 0.2))
        if r.random() < 0.08:
            ops.append({"op": r.choice(["status", "history", "hook"]), "db": "main", "provider": "claude",
                        "now": iso(at + timedelta(minutes=r.choice([0, 30, 130])), r), "limit": 1000})
    last = start + timedelta(minutes=timeline[-1])
    for later in (0, 1, 61, 400):
        ops.append({"op": "status", "db": "main", "provider": "claude", "now": iso(last + timedelta(minutes=later), r)})
    return ops

LEGACY_SCHEMA = """CREATE TABLE observations (
    id INTEGER PRIMARY KEY, provider TEXT NOT NULL, observed_at TEXT NOT NULL,
    weekly_used REAL NOT NULL, weekly_reset_at TEXT NOT NULL, short_used REAL,
    short_reset_at TEXT, source TEXT NOT NULL, schema_marker TEXT NOT NULL);
CREATE INDEX observations_by_provider_time ON observations(provider, observed_at);
CREATE TABLE governor_state (provider TEXT PRIMARY KEY, window_reset_at TEXT, base_mode TEXT,
    last_change_at TEXT, candidate_mode TEXT, candidate_first_at TEXT, candidate_last_at TEXT);
PRAGMA user_version = 1;"""

def gen_legacy(seed):
    r = random.Random(seed)
    now = datetime(2026, 10, 23, 12, tzinfo=UTC) + timedelta(minutes=r.randrange(0, 300))
    reset = now + timedelta(hours=r.uniform(100, 166))
    ops = []
    kind = r.choice(["legacy", "duplicate", "uncertain"])
    if kind == "legacy":
        rows = r.choice([10, 63, 64, 65, 90])
        start = now - timedelta(minutes=rows + 1)
        values = ",".join(
            f"('codex','{iso(start + timedelta(minutes=i), r)}',{10 + i / 100},'{iso(reset, r)}',NULL,NULL,'manual','manual-v1')"
            for i in range(rows))
        ops.append({"op": "sql", "sql": LEGACY_SCHEMA + "INSERT INTO observations (provider, observed_at, weekly_used, "
                    "weekly_reset_at, short_used, short_reset_at, source, schema_marker) VALUES " + values + ";"})
        provider = "codex"
    else:
        provider = "claude"
        for i in range(70):
            at = now - timedelta(minutes=5 * (69 - i))
            ops.append({"op": "record", "db": "main", "provider": provider, "observed_at": iso(at, r),
                        "weekly": 20.0 if i == 0 else 19.0 + r.choice([0, 0, 0.1]) * i, "reset": iso(reset, r),
                        "short": None, "short_reset": None, "now": iso(at, r)})
        if kind == "duplicate":
            ops.append({"op": "sql", "sql": "INSERT INTO observations (provider, observed_at, weekly_used, weekly_reset_at, "
                        "short_used, short_reset_at, source, schema_marker) SELECT provider, observed_at, weekly_used, "
                        "weekly_reset_at, short_used, short_reset_at, source, schema_marker FROM observations "
                        "WHERE provider='claude' LIMIT 1;"})
        else:
            until = iso(now + timedelta(hours=r.choice([1, 24])), r)
            ops.append({"op": "sql", "sql": f"UPDATE governor_state SET history_uncertain_until='{until}', "
                        f"candidate_mode='Rich', candidate_first_at='{iso(now, r)}', candidate_last_at='{iso(now, r)}';"})
    for step in range(r.choice([3, 30, 80])):
        at = now + timedelta(minutes=10 * (step + 1))
        ops.append({"op": "record", "db": "main", "provider": provider, "observed_at": iso(at, r),
                    "weekly": 21.0 + step * r.choice([0.05, 0.2]), "reset": iso(reset, r),
                    "short": None, "short_reset": None, "now": iso(at + timedelta(minutes=r.choice([0, 0, 130])), r)})
        if step % 5 == 0:
            ops.append({"op": "status", "db": "main", "provider": provider, "now": iso(at, r)})
    ops.append({"op": "history", "db": "main", "provider": provider, "now": iso(at, r), "limit": 1000})
    return ops

def dump_db(path):
    if not os.path.exists(path):
        return None
    c = sqlite3.connect(path)
    try:
        return (c.execute("select * from observations order by id").fetchall(),
                c.execute("select * from governor_state order by provider").fetchall(),
                c.execute("pragma user_version").fetchone())
    finally:
        c.close()

def run(cmd, ops, directory):
    os.makedirs(directory, exist_ok=True)
    lines, chunk = [], []
    for op in ops + [None]:
        if op is not None and op["op"] != "sql":
            chunk.append(op)
            continue
        if chunk:
            text = "".join(json.dumps(item) + "\n" for item in chunk)
            out = subprocess.run(cmd + [directory], input=text, capture_output=True, text=True, timeout=600)
            if out.returncode:
                raise SystemExit(f"{cmd[0]} failed: {out.stderr[-2000:]}")
            lines += out.stdout.splitlines()
            chunk = []
        if op is not None:
            c = sqlite3.connect(f"{directory}/main.sqlite3")
            c.executescript(op["sql"]); c.commit(); c.close()
            lines.append("sql")
    return lines

def main(seeds):
    root = tempfile.mkdtemp(prefix="demon-parity-")
    total_ops = mismatches = 0
    import collections
    reasons = collections.Counter()
    for seed in seeds:
        ops = gen_legacy(seed) if seed >= 2000 else gen_dense(seed) if seed >= 1000 else gen(seed)
        total_ops += len(ops)
        py = run([sys.executable, PY_HARNESS], ops, f"{root}/{seed}/py")
        rs = run([RUST], ops, f"{root}/{seed}/rs")
        for line in py:
            if line.startswith("{") and '"reason"' in line:
                reasons[json.loads(line)["reason"]] += 1
        bad = [(i, ops[i], a, b) for i, (a, b) in enumerate(zip(py, rs)) if a != b]
        db_same = dump_db(f"{root}/{seed}/py/main.sqlite3") == dump_db(f"{root}/{seed}/rs/main.sqlite3")
        # Hand-off: Python writes the first half, Rust continues on the same ledger file.
        half = len(ops) // 2
        run([sys.executable, PY_HARNESS], ops[:half], f"{root}/{seed}/mix")
        mixed = run([RUST], ops[half:], f"{root}/{seed}/mix")
        handoff_same = mixed == py[half:] and dump_db(f"{root}/{seed}/mix/main.sqlite3") == dump_db(f"{root}/{seed}/py/main.sqlite3")
        if bad or not db_same or len(py) != len(rs) or not handoff_same:
            mismatches += 1
            print(f"seed {seed}: {len(bad)} output diffs, db_same={db_same}, handoff_same={handoff_same}")
            for i, op, a, b in bad[:3]:
                print("  op", i, json.dumps(op)); print("  py", a[:400]); print("  rs", b[:400])
    print("reasons:", dict(sorted(reasons.items())))
    print(f"{len(seeds)} scenarios, {total_ops} operations, {mismatches} mismatching scenarios")
    shutil.rmtree(root, ignore_errors=True)
    return mismatches

if __name__ == "__main__":
    seeds = []
    for spec in sys.argv[1:]:
        start, stop = map(int, spec.split(":"))
        seeds += range(start, stop)
    sys.exit(1 if main(seeds) else 0)
