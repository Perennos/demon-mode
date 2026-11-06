// Developer-only: replays operations from tools/parity/compare.py against this crate with explicit
// clocks, printing one compact JSON line per operation.
use demon_mode::governor::{Provider, Reading, Val, hook_context};
use demon_mode::{hook, pyjson, store, ts};
use serde_json::{Value, json};
use std::io::BufRead;

fn ts_of(value: &Value) -> ts::Ts {
    demon_mode::governor::parse_timestamp(Val::from_json(value).as_ref()).expect("op time")
}

fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let op: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let db = format!("{dir}/{}.sqlite3", op["db"].as_str().unwrap());
        let db = Some(db.as_str());
        let now = ts_of(&op["now"]);
        let provider = Provider::parse(op["provider"].as_str().unwrap_or("claude")).unwrap();
        let out = match op["op"].as_str().unwrap() {
            "record" => {
                let reading = Reading {
                    provider: Val::from_json(&op["provider"]),
                    observed_at: Val::from_json(&op["observed_at"]),
                    weekly_used: Val::from_json(&op["weekly"]),
                    weekly_reset_at: Val::from_json(&op["reset"]),
                    short_used: Val::from_json(&op["short"]),
                    short_reset_at: Val::from_json(&op["short_reset"]),
                };
                match store::record_manual(db, &reading, now) {
                    Ok(policy) => policy.as_json(now),
                    Err(error) => json!({"error": error.to_string()}),
                }
            }
            "status" => store::load_policy(db, provider, now).as_json(now),
            "context" => Value::String(hook_context(&store::load_policy(db, provider, now), 2048)),
            "hook" => Value::String(hook::hook_json(&store::load_policy(db, provider, now))),
            "history" => match store::load_history(db, provider, op["limit"].as_i64().unwrap(), now) {
                Ok(items) => Value::Array(items.iter().map(|item| json!([
                    item.observed_at.iso(), pyjson::float(item.weekly_used), item.weekly_reset_at.iso(),
                    pyjson::opt_float(item.short_used), item.short_reset_at.map(|t| t.iso()), item.source.name(), item.schema_marker,
                ])).collect()),
                Err(error) => json!({"error": error.to_string()}),
            },
            other => panic!("unknown op {other}"),
        };
        println!("{}", pyjson::compact(&out));
    }
}
