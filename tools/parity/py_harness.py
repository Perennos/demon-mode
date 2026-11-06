"""Replays operations against the Python 0.1.1 library; one compact JSON line per operation.

Developer-only. DEMON_PY_SRC must name a checkout of the Python release (for example
`git worktree add ../demon-py 69c93dd`).
"""
import json, os, sys
sys.path.insert(0, os.environ["DEMON_PY_SRC"])
import demon_mode.__main__ as cli
from demon_mode.governor import hook_context, parse_timestamp
from demon_mode.store import load_history, load_policy, record_manual

def dump(value):
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":"))

directory = sys.argv[1]
for line in sys.stdin:
    op = json.loads(line)
    db = f"{directory}/{op['db']}.sqlite3"
    now = parse_timestamp(op["now"])
    provider = op.get("provider", "claude")
    kind = op["op"]
    if kind == "record":
        try:
            out = record_manual(db, provider=op["provider"], observed_at=op["observed_at"], weekly_used=op["weekly"],
                                weekly_reset_at=op["reset"], short_used=op["short"], short_reset_at=op["short_reset"],
                                now=now).as_dict(now)
        except ValueError as exc:
            out = {"error": str(exc)}
    elif kind == "status":
        out = load_policy(db, provider, now=now).as_dict(now)
    elif kind == "context":
        out = hook_context(load_policy(db, provider, now=now), now)
    elif kind == "hook":
        out = cli._hook_json(load_policy(db, provider, now=now), now)
    elif kind == "history":
        try:
            out = [[i.observed_at.isoformat().replace("+00:00", "Z"), i.weekly_used, i.weekly_reset_at.isoformat().replace("+00:00", "Z"),
                    i.short_used, i.short_reset_at.isoformat().replace("+00:00", "Z") if i.short_reset_at else None, i.source, i.schema_marker]
                   for i in load_history(db, provider, limit=op["limit"], now=now)]
        except ValueError as exc:
            out = {"error": str(exc)}
    print(dump(out))
