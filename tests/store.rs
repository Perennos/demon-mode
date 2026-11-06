//! Ported from tests/test_store.py.
mod common;

use common::*;
use demon_mode::governor::{
    CONFLICT_EQUAL_TIME, Mode, Observation, Provider, Reading, Source, Val, evaluate_policy, is_conflict,
    normalize_observation, weekly_metrics,
};
use demon_mode::store::{load_history, load_policy, peek_policy};
use demon_mode::ts::{Ts, parse_text};

fn state_row(db: &str, columns: &str) -> Vec<Option<String>> {
    let count = columns.split(',').count();
    sql(db)
        .query_row(&format!("SELECT {columns} FROM governor_state WHERE provider='claude'"), [], |row| {
            (0..count).map(|index| row.get(index)).collect()
        })
        .unwrap()
}

fn count_rows(db: &str, provider: &str) -> i64 {
    sql(db).query_row("SELECT COUNT(*) FROM observations WHERE provider=?", [provider], |row| row.get(0)).unwrap()
}

#[test]
fn status_reads_do_not_promote_or_mutate_candidate_state() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    record(&db, "claude", NOW, 0.2, NOW + 20 * HOUR, Some((2.0, NOW + 4 * HOUR)), NOW);
    sql(&db)
        .execute(
            "UPDATE governor_state SET base_mode='Normal', last_change_at=?, candidate_mode='Rich', \
             candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
            [(NOW - 2 * HOUR).iso(), NOW.iso(), (NOW + HOUR).iso()],
        )
        .unwrap();
    let columns = "base_mode, candidate_mode, candidate_first_at, candidate_last_at";
    let before = state_row(&db, columns);
    let run = invoke_at(&["--db", &db, "status", "--provider", "claude", "--json"], NOW + 3 * HOUR);
    assert_eq!(run.code, 0);
    assert_eq!(before, state_row(&db, columns));
}

#[test]
fn decrease_conflict_preserves_peak_and_clears_candidate() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let reset = NOW + 100 * HOUR;
    record(&db, "claude", NOW, 20.0, reset, Some((1.0, NOW + 4 * HOUR)), NOW);
    sql(&db)
        .execute(
            "UPDATE governor_state SET candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
            [NOW.iso(), NOW.iso()],
        )
        .unwrap();
    let later = NOW + HOUR;
    let policy = record(&db, "claude", later, 19.0, reset, Some((1.0, NOW + 4 * HOUR)), later);
    assert_eq!(policy.weekly_used, Some(20.0));
    assert_eq!(policy.reason, "reading_conflict_refresh_required");
    assert_eq!(state_row(&db, "candidate_mode, candidate_first_at, candidate_last_at"), vec![None, None, None]);
    let markers: Vec<String> = sql(&db)
        .prepare("SELECT schema_marker FROM observations")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(markers.iter().any(|marker| marker == "conflict-decreasing-v1"));
}

#[test]
fn repeated_decreases_compare_with_independent_window_peaks() {
    let cases = [
        ("weekly", [20.0, 19.0, 19.5, 20.1], [10.0, 10.0, 10.0, 10.0]),
        ("short", [20.0, 20.1, 20.2, 20.3], [20.0, 19.0, 19.5, 20.1]),
    ];
    for (bucket, weekly, short) in cases {
        let temp = TempDir::new();
        let db = temp.db("state.sqlite3");
        for (index, (week_used, short_used)) in weekly.iter().zip(short).enumerate() {
            let at = NOW + index as i64 * HOUR;
            if index == 1 {
                sql(&db)
                    .execute(
                        "UPDATE governor_state SET candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? \
                         WHERE provider='claude'",
                        [NOW.iso(), NOW.iso()],
                    )
                    .unwrap();
            }
            record(&db, "claude", at, *week_used, NOW + 100 * HOUR, Some((short_used, NOW + 4 * HOUR)), at);
            if index == 1 || index == 2 {
                let history = load_history(Some(&db), Provider::Claude, 20, at).unwrap();
                assert!(history.iter().any(|item| item.schema_marker == "conflict-decreasing-v1"), "{bucket}");
                assert_eq!(state_row(&db, "candidate_mode"), vec![None], "{bucket}");
            }
        }
        let history = load_history(Some(&db), Provider::Claude, 20, NOW + 3 * HOUR).unwrap();
        assert!(!is_conflict(&history[0].schema_marker), "{bucket}");
        assert_eq!(history[0].weekly_used, weekly[3]);
        assert_eq!(history[0].short_used, Some(short[3]));
    }
}

#[test]
fn equal_timestamp_disagreement_is_a_conflict() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let reset = NOW + 100 * HOUR;
    record(&db, "claude", NOW, 20.0, reset, Some((1.0, NOW + 4 * HOUR)), NOW);
    let policy = record(&db, "claude", NOW, 21.0, reset, Some((1.0, NOW + 4 * HOUR)), NOW);
    assert_eq!(policy.reason, "reading_conflict_refresh_required");
    let history = load_history(Some(&db), Provider::Claude, 20, NOW).unwrap();
    assert_eq!(history.len(), 2);
    assert!(is_conflict(&history[0].schema_marker));
}

#[test]
fn out_of_order_input_does_not_replace_newest_or_advance_state() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let reset = NOW + 100 * HOUR;
    record(&db, "codex", NOW, 20.0, reset, None, NOW);
    let newest = NOW + 2 * HOUR;
    record(&db, "codex", newest, 30.0, reset, None, newest);
    let read = |db: &str| -> (Option<String>, Option<String>, Option<String>) {
        sql(db)
            .query_row("SELECT base_mode, candidate_mode, candidate_last_at FROM governor_state", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
    };
    let before = read(&db);
    record(&db, "codex", NOW + HOUR, 25.0, reset, None, newest);
    let policy = load_policy(Some(&db), Provider::Codex, newest);
    assert_eq!(policy.weekly_used, Some(30.0));
    assert_eq!(before, read(&db));
    assert_eq!(load_history(Some(&db), Provider::Codex, 20, newest).unwrap().len(), 3);
}

#[test]
fn provider_histories_and_policy_state_are_separate() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    for (provider, amount) in [("claude", 12.0), ("codex", 78.0)] {
        record(&db, provider, NOW, amount, NOW + 100 * HOUR, None, NOW);
    }
    assert_eq!(load_policy(Some(&db), Provider::Claude, NOW).weekly_used, Some(12.0));
    assert_eq!(load_policy(Some(&db), Provider::Codex, NOW).weekly_used, Some(78.0));
    assert_eq!(load_history(Some(&db), Provider::Claude, 20, NOW).unwrap().len(), 1);
    assert_eq!(load_history(Some(&db), Provider::Codex, 20, NOW).unwrap().len(), 1);
}

#[test]
fn observation_history_is_bounded_and_keeps_window_peak() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let reset = NOW + 100 * HOUR;
    for index in 0..70 {
        let at = NOW - 5 * MINUTE * (69 - index);
        record(&db, "claude", at, if index == 0 { 20.0 } else { 19.0 }, reset, None, at);
    }
    assert!(count_rows(&db, "claude") <= 64);
    let history = load_history(Some(&db), Provider::Claude, 1000, NOW).unwrap();
    assert!(history.len() <= 64);
    let policy = load_policy(Some(&db), Provider::Claude, NOW);
    assert_eq!(policy.weekly_used, Some(20.0));
    assert!(is_conflict(&history[0].schema_marker));
    assert_eq!(policy.mode, Mode::Lean);
    assert!(policy.refresh_required);
    sql(&db)
        .execute(
            "INSERT INTO observations (provider, observed_at, weekly_used, weekly_reset_at, short_used, \
             short_reset_at, source, schema_marker) SELECT provider, observed_at, weekly_used, weekly_reset_at, \
             short_used, short_reset_at, source, schema_marker FROM observations WHERE provider='claude' LIMIT 1",
            [],
        )
        .unwrap();
    let incomplete = load_policy(Some(&db), Provider::Claude, NOW);
    assert_eq!(incomplete.mode, Mode::Lean);
    assert_eq!(incomplete.reason, "state_history_incomplete");
    assert!(incomplete.refresh_required);

    let pace_db = temp.db("pace.sqlite3");
    for index in 0..70 {
        let at = NOW - 10 * MINUTE * (69 - index);
        record(&pace_db, "codex", at, 20.0 + index as f64 * 0.1, reset, None, at);
    }
    let pace = load_policy(Some(&pace_db), Provider::Codex, NOW).pace_per_hour.unwrap();
    let expected = (26.9 / 68.0 + 6.9 / 11.5) / 2.0;
    assert!((pace - expected).abs() < 1e-9, "{pace} != {expected}");
}

struct Dense {
    db: String,
    start: Ts,
    week_reset: Ts,
    raw: Vec<Observation>,
}

fn same_week(item: &Observation, reset: Ts) -> bool {
    (item.weekly_reset_at - reset).abs() <= 2 * SECOND
}

fn max_by_time(items: &[&Observation]) -> Observation {
    let mut best = items[0];
    for item in items {
        if item.observed_at > best.observed_at {
            best = item;
        }
    }
    best.clone()
}

impl Dense {
    fn check_invariants(&self, at: Ts) {
        assert!(count_rows(&self.db, "claude") <= 64);
        let stored = load_history(Some(&self.db), Provider::Claude, 1000, at).unwrap();
        assert!(stored.len() <= 64);
        let accepted_raw: Vec<&Observation> = self.raw.iter().filter(|i| !is_conflict(&i.schema_marker)).collect();
        let accepted_stored: Vec<&Observation> = stored.iter().filter(|i| !is_conflict(&i.schema_marker)).collect();
        if accepted_raw.is_empty() {
            return;
        }
        let latest_raw = max_by_time(&accepted_raw);
        let raw_week: Vec<&Observation> =
            accepted_raw.iter().copied().filter(|i| same_week(i, latest_raw.weekly_reset_at)).collect();
        let stored_week: Vec<&Observation> =
            accepted_stored.iter().copied().filter(|i| same_week(i, latest_raw.weekly_reset_at)).collect();
        assert!(accepted_stored.contains(&&latest_raw));
        let peak = |items: &[&Observation]| items.iter().map(|i| i.weekly_used).fold(f64::MIN, f64::max);
        let earliest = |items: &[&Observation]| items.iter().map(|i| i.weekly_reset_at).min();
        assert_eq!(peak(&raw_week), peak(&stored_week));
        assert_eq!(earliest(&raw_week), earliest(&stored_week));
        let raw_short: Vec<&Observation> = raw_week.iter().copied().filter(|i| i.short_reset_at.is_some()).collect();
        let stored_short: Vec<&Observation> =
            accepted_stored.iter().copied().filter(|i| i.short_reset_at.is_some()).collect();
        if !raw_short.is_empty() {
            let latest_short = max_by_time(&raw_short).short_reset_at.unwrap();
            let window = |items: &[&Observation]| -> Vec<(f64, Ts)> {
                items
                    .iter()
                    .filter(|i| (i.short_reset_at.unwrap() - latest_short).abs() <= 2 * SECOND)
                    .map(|i| (i.short_used.unwrap(), i.short_reset_at.unwrap()))
                    .collect()
            };
            let (raw_window, stored_window) = (window(&raw_short), window(&stored_short));
            let short_peak = |w: &[(f64, Ts)]| w.iter().map(|x| x.0).fold(f64::MIN, f64::max);
            assert_eq!(short_peak(&raw_window), short_peak(&stored_window));
            assert_eq!(raw_window.iter().map(|x| x.1).min(), stored_window.iter().map(|x| x.1).min());
        }
        let latest_conflict =
            |items: &[Observation]| items.iter().filter(|i| is_conflict(&i.schema_marker)).map(|i| i.observed_at).max();
        if latest_conflict(&self.raw).is_some() {
            assert_eq!(latest_conflict(&self.raw), latest_conflict(&stored));
        }
        let reference = weekly_metrics(&self.raw, at);
        let bounded = weekly_metrics(&stored, at);
        if reference.credible_pace {
            assert!(
                bounded.credible_pace
                    || bounded.recent_pace_upper_bound_per_hour.is_some()
                    || load_policy(Some(&self.db), Provider::Claude, at).reason == "state_history_incomplete"
            );
        }
    }

    fn add(&mut self, minute: i64, now: Option<Ts>) {
        let at = self.start + minute * MINUTE;
        let current = now.unwrap_or(at);
        let reset = match minute {
            0 => self.week_reset + 2 * SECOND,
            120 => self.week_reset,
            _ => self.week_reset + SECOND,
        };
        let short_reset = self.start + (minute / 300 + 1) * 300 * MINUTE;
        let weekly_used = 3.2 + minute as f64 * 0.0066666666667;
        let short_used = 30.0 + (minute % 300) as f64 * 0.02;
        let short = Some((short_used, short_reset));
        self.raw.push(observation(at, weekly_used, Some(reset), short, Some(current)));
        record(&self.db, "claude", at, weekly_used, reset, short, current);
        self.check_invariants(current);
        if minute == 240 {
            let shifted = Some((short_used + 0.2, short_reset));
            let mut conflict = observation(at, weekly_used + 0.2, Some(reset), shifted, Some(at));
            record(&self.db, "claude", at, weekly_used + 0.2, reset, shifted, at);
            conflict.schema_marker = CONFLICT_EQUAL_TIME.to_string();
            self.raw.push(conflict);
            self.check_invariants(at);
        }
    }
}

#[test]
fn dense_rollover_preserves_bounds_and_recovers_uncertain_history() {
    let temp = TempDir::new();
    let mut dense = Dense { db: temp.db("dense.sqlite3"), start: NOW, week_reset: NOW + 160 * HOUR, raw: Vec::new() };
    let start = dense.start;
    let mut timeline = vec![0];
    timeline.extend(60..120);
    timeline.extend((120..=24 * 60).step_by(60));
    timeline.extend(24 * 60 + 20..=25 * 60 + 30);
    for minute in timeline {
        dense.add(minute, None);
    }
    let db = dense.db.clone();
    let expired_old: Option<i64> = sql(&db)
        .query_row("SELECT 1 FROM observations WHERE provider='claude' AND observed_at=?", [start.iso()], |row| {
            row.get(0)
        })
        .ok();
    assert_eq!(expired_old, None);
    let latest = start + 25 * HOUR + 30 * MINUTE;
    let bounded = load_policy(Some(&db), Provider::Claude, latest);
    let reference = evaluate_policy(Provider::Claude, &dense.raw, None, latest, None);
    assert!(bounded.recent_pace_upper_bound_per_hour.is_some());
    assert!(bounded.pace_per_hour >= reference.pace_per_hour);
    assert!(bounded.ratio <= reference.ratio);
    assert!(bounded.mode.rank() <= reference.mode.rank());
    assert_eq!(
        bounded.as_json(latest)["weekly_recent_pace_upper_bound_percent_per_hour"],
        demon_mode::pyjson::opt_float(bounded.recent_pace_upper_bound_per_hour)
    );
    assert_eq!(state_row(&db, "history_uncertain_until"), vec![None]);

    for minute in 25 * 60 + 31..26 * 60 + 2 {
        dense.add(minute, None);
    }
    let latest = start + 26 * HOUR + MINUTE;
    let fresh = load_policy(Some(&db), Provider::Claude, latest);
    assert_ne!(fresh.reason, "state_history_incomplete");
    assert!(fresh.mode.rank() > Mode::Lean.rank());

    let uncertain_until = latest + 24 * HOUR;
    let until_stamp = uncertain_until.iso();
    sql(&db)
        .execute(
            "UPDATE governor_state SET history_uncertain_until=?, candidate_mode='Rich', candidate_first_at=?, \
             candidate_last_at=? WHERE provider='claude'",
            [until_stamp.clone(), latest.iso(), latest.iso()],
        )
        .unwrap();
    let held = load_policy(Some(&db), Provider::Claude, latest);
    assert_eq!(held.reason, "state_history_incomplete");
    assert!(held.mode.rank() <= Mode::Lean.rank());
    let base_minute = (latest - start) / MINUTE;
    for hour in 1..24 {
        dense.add(base_minute + hour * 60, None);
        assert_eq!(state_row(&db, "history_uncertain_until, candidate_mode"), vec![Some(until_stamp.clone()), None]);
    }
    let after_deadline = load_policy(Some(&db), Provider::Claude, uncertain_until + MINUTE);
    assert_eq!(after_deadline.reason, "state_history_incomplete");
    let stale_at = uncertain_until + MINUTE;
    let stale_now = stale_at + 2 * HOUR;
    dense.add((stale_at - start) / MINUTE, Some(stale_now));
    let state = state_row(&db, "history_uncertain_until, candidate_mode, candidate_first_at, candidate_last_at");
    let stale_latest = load_history(Some(&db), Provider::Claude, 1, stale_now).unwrap().remove(0);
    let stale_policy = load_policy(Some(&db), Provider::Claude, stale_now);
    assert_eq!(stale_latest.observed_at, stale_at);
    assert_eq!(stale_now - stale_latest.observed_at, 2 * HOUR);
    assert_eq!(state, vec![Some(until_stamp.clone()), None, None, None]);
    assert_eq!(stale_policy.reason, "state_history_incomplete");
    assert!(stale_policy.mode.rank() <= Mode::Lean.rank());

    let recovered_at = stale_now + MINUTE;
    dense.add((recovered_at - start) / MINUTE, Some(recovered_at));
    assert_eq!(state_row(&db, "history_uncertain_until"), vec![None]);
    assert_ne!(load_policy(Some(&db), Provider::Claude, recovered_at).reason, "state_history_incomplete");
    let stored = load_history(Some(&db), Provider::Claude, 1000, recovered_at).unwrap();
    assert!(weekly_metrics(&stored, recovered_at).credible_pace);

    let pristine_db = temp.db("pristine.sqlite3");
    let pristine_reset = NOW + 120 * HOUR;
    let pristine = observation(NOW, 20.0, Some(pristine_reset), None, Some(NOW));
    record(&pristine_db, "codex", NOW, 20.0, pristine_reset, None, NOW);
    let pristine_metrics = weekly_metrics(&[pristine], NOW);
    assert!(!pristine_metrics.credible_pace);
    assert_eq!(pristine_metrics.recent_pace_upper_bound_per_hour, None);
    assert!((pristine_metrics.pace_per_hour.unwrap() - 20.0 / 48.0).abs() < 1e-12);
    let uncertain: Option<String> = sql(&pristine_db)
        .query_row("SELECT history_uncertain_until FROM governor_state WHERE provider='codex'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(uncertain, None);

    let legacy_db = temp.db("legacy-v1.sqlite3");
    let legacy_start = NOW - 63 * MINUTE;
    let legacy_reset = NOW + 166 * HOUR;
    let connection = sql(&legacy_db);
    connection
        .execute_batch(
            "CREATE TABLE observations (
                 id INTEGER PRIMARY KEY, provider TEXT NOT NULL, observed_at TEXT NOT NULL,
                 weekly_used REAL NOT NULL, weekly_reset_at TEXT NOT NULL, short_used REAL,
                 short_reset_at TEXT, source TEXT NOT NULL, schema_marker TEXT NOT NULL);
             CREATE INDEX observations_by_provider_time ON observations(provider, observed_at);
             CREATE TABLE governor_state (
                 provider TEXT PRIMARY KEY, window_reset_at TEXT, base_mode TEXT,
                 last_change_at TEXT, candidate_mode TEXT, candidate_first_at TEXT,
                 candidate_last_at TEXT);
             PRAGMA user_version = 1;",
        )
        .unwrap();
    for index in 0..64 {
        connection
            .execute(
                "INSERT INTO observations (provider, observed_at, weekly_used, weekly_reset_at, short_used, \
                 short_reset_at, source, schema_marker) VALUES ('codex', ?, ?, ?, NULL, NULL, 'manual', 'manual-v1')",
                rusqlite::params![
                    (legacy_start + index * MINUTE).iso(),
                    10.0 + index as f64 / 100.0,
                    legacy_reset.iso()
                ],
            )
            .unwrap();
    }
    drop(connection);
    assert_eq!(load_policy(Some(&legacy_db), Provider::Codex, NOW).reason, "state_history_incomplete");
    let connection = sql(&legacy_db);
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    let until: String = connection
        .query_row("SELECT history_uncertain_until FROM governor_state WHERE provider='codex'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(parse_text(&until), Ok(NOW + 24 * HOUR));
}

#[test]
fn corrupt_and_locked_state_fall_back_without_erasure() {
    let temp = TempDir::new();
    let corrupt = temp.db("corrupt.sqlite3");
    std::fs::write(&corrupt, b"not a sqlite database").unwrap();
    let policy = load_policy(Some(&corrupt), Provider::Claude, NOW);
    assert_eq!((policy.mode, policy.reason), (Mode::Lean, "state_unavailable"));
    assert_eq!(std::fs::read(&corrupt).unwrap(), b"not a sqlite database");

    let locked = temp.db("locked.sqlite3");
    load_policy(Some(&locked), Provider::Claude, NOW);
    let connection = sql(&locked);
    connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let fallback = load_policy(Some(&locked), Provider::Claude, NOW);
    assert_eq!((fallback.mode, fallback.reason), (Mode::Lean, "state_unavailable"));
    connection.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn linked_state_paths_are_rejected_and_newer_schema_is_unchanged() {
    let temp = TempDir::new();
    let newer = temp.db("newer.sqlite3");
    let connection = sql(&newer);
    connection
        .execute_batch(
            "PRAGMA user_version = 99; CREATE TABLE marker (value TEXT); INSERT INTO marker VALUES ('keep');",
        )
        .unwrap();
    drop(connection);
    let before = std::fs::read(&newer).unwrap();
    let fallback = load_policy(Some(&newer), Provider::Claude, NOW);
    assert_eq!((fallback.mode, fallback.reason), (Mode::Lean, "state_unavailable"));
    assert_eq!(std::fs::read(&newer).unwrap(), before);

    let directory = temp.path().join("directory.sqlite3");
    std::fs::create_dir(&directory).unwrap();
    let found = load_policy(directory.to_str(), Provider::Claude, NOW);
    assert_eq!((found.mode, found.reason), (Mode::Lean, "state_unavailable"));
    let parent_file = temp.path().join("not-a-directory");
    std::fs::write(&parent_file, b"preserve").unwrap();
    let found = load_policy(parent_file.join("state.sqlite3").to_str(), Provider::Claude, NOW);
    assert_eq!((found.mode, found.reason), (Mode::Lean, "state_unavailable"));
    assert_eq!(std::fs::read(&parent_file).unwrap(), b"preserve");

    #[cfg(unix)]
    {
        let target = temp.path().join("target.sqlite3");
        std::fs::write(&target, &before).unwrap();
        let linked = temp.path().join("linked.sqlite3");
        std::os::unix::fs::symlink(&target, &linked).unwrap();
        let found = load_policy(linked.to_str(), Provider::Claude, NOW);
        assert_eq!((found.mode, found.reason), (Mode::Lean, "state_unavailable"));
        assert_eq!(std::fs::read(&target).unwrap(), before);

        let real_parent = temp.path().join("real-parent");
        std::fs::create_dir(&real_parent).unwrap();
        let linked_parent = temp.path().join("linked-parent");
        std::os::unix::fs::symlink(&real_parent, &linked_parent).unwrap();
        let found = load_policy(linked_parent.join("child.sqlite3").to_str(), Provider::Claude, NOW);
        assert_eq!((found.mode, found.reason), (Mode::Lean, "state_unavailable"));
        assert!(!real_parent.join("child.sqlite3").exists());
    }
}

#[test]
fn invalid_manual_schema_values_are_rejected() {
    let base = || Reading {
        provider: Some(Val::text("claude")),
        observed_at: Some(Val::Time(NOW)),
        weekly_used: Some(Val::Num(10.0)),
        weekly_reset_at: Some(Val::Time(NOW + 100 * HOUR)),
        ..Reading::default()
    };
    let bad = [
        Reading { weekly_used: Some(Val::Bool(true)), ..base() },
        Reading { weekly_used: Some(Val::Num(f64::NAN)), ..base() },
        Reading { weekly_used: Some(Val::Num(f64::INFINITY)), ..base() },
        Reading { observed_at: Some(Val::text("2026-10-23T12:00:00")), ..base() },
        Reading { weekly_reset_at: Some(Val::Time(NOW - SECOND)), ..base() },
    ];
    for reading in bad {
        assert!(normalize_observation(&reading, Source::Manual, NOW).is_err(), "{reading:?}");
    }
    assert!(normalize_observation(&base(), Source::Manual, NOW).is_ok());
}

#[test]
fn readings_dated_more_than_a_minute_ahead_are_rejected() {
    let ahead = |seconds: i64| Reading {
        observed_at: Some(Val::Time(NOW + seconds * SECOND)),
        ..reading("claude", NOW, 10.0, NOW + 100 * HOUR, None)
    };
    assert!(normalize_observation(&ahead(60), Source::Manual, NOW).is_ok());
    let error = normalize_observation(&ahead(61), Source::Manual, NOW).unwrap_err();
    assert_eq!(error.to_string(), "observation is in the future");
}

#[test]
fn freshness_caps_follow_the_one_and_six_hour_rules() {
    let reset = NOW + 100 * HOUR;
    let short = |at: Ts| Some((5.0, at + 4 * HOUR));
    let fresh = observation(NOW, 30.0, Some(reset), short(NOW), Some(NOW));
    let policy = evaluate_policy(Provider::Claude, std::slice::from_ref(&fresh), None, NOW, None);
    assert_eq!(policy.mode, Mode::Rich);
    let aged = observation(NOW - 2 * HOUR, 30.0, Some(reset), short(NOW - 2 * HOUR), Some(NOW));
    let policy = evaluate_policy(Provider::Claude, &[aged], None, NOW, None);
    assert!(policy.mode.rank() <= Mode::Normal.rank() && policy.refresh_required);
    let stale = observation(NOW - 7 * HOUR, 30.0, Some(reset), None, Some(NOW));
    let policy = evaluate_policy(Provider::Claude, &[stale], None, NOW, None);
    assert_eq!((policy.mode, policy.reason, policy.weekly_used), (Mode::Lean, "weekly_observation_stale", None));
}

#[test]
fn peek_policy_never_creates_a_ledger() {
    let temp = TempDir::new();
    let db = temp.db("absent.sqlite3");
    let (policy, present) = peek_policy(Some(&db), Provider::Codex, NOW);
    assert!(!present);
    assert_eq!(policy.mode, Mode::Lean);
    assert!(!std::path::Path::new(&db).exists());
}
