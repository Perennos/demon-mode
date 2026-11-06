//! Ported from tests/test_governor.py.
mod common;

use common::*;
use demon_mode::governor::{
    GovernorState, MODES, Mode, Provider, WeeklyMetrics, advance_state, evaluate_policy, weekly_metrics,
};
use demon_mode::ts::Ts;
use std::collections::HashSet;

fn metrics(reason: &'static str, deadline: Ts, ratio: f64, raw: Mode, age: Option<i64>) -> WeeklyMetrics {
    WeeklyMetrics {
        available: true,
        deadline: Some(deadline),
        ratio: Some(ratio),
        raw_mode: raw,
        age,
        ..WeeklyMetrics::unavailable(reason)
    }
}

#[test]
fn five_mode_instructions_are_distinct() {
    let instructions: HashSet<&str> = MODES.iter().map(|mode| mode.instruction()).collect();
    assert_eq!(MODES.len(), 5);
    assert_eq!(instructions.len(), 5);
}

#[test]
fn threshold_bands_first_twelve_hours_and_last_twelve_hour_target() {
    for (used, expected) in [(55.0, Mode::Austere), (42.0, Mode::Lean), (36.0, Mode::Normal), (30.0, Mode::Rich)] {
        let found = weekly_metrics(&[observation(NOW, used, None, None, None)], NOW);
        assert_eq!(found.raw_mode, expected, "used={used}");
    }
    let early = weekly_metrics(&[observation(NOW, 1.0, Some(NOW + 164 * HOUR), None, None)], NOW);
    assert_eq!(early.raw_mode, Mode::Normal);
    assert!(!early.credible_pace);
    let last_12 = weekly_metrics(&[observation(NOW, 1.0, Some(NOW + 12 * HOUR), None, None)], NOW);
    let outside_12 = weekly_metrics(&[observation(NOW, 1.0, Some(NOW + 13 * HOUR), None, None)], NOW);
    assert_eq!(last_12.target, Some(97.0));
    assert_eq!(outside_12.target, Some(92.0));
}

#[test]
fn blended_pace_and_delayed_read_estimate() {
    let deadline = NOW + 68 * HOUR;
    let first = observation(NOW - 6 * HOUR, 14.0, Some(deadline), None, None);
    let latest = observation(NOW, 20.0, Some(deadline), None, None);
    let found = weekly_metrics(&[first, latest], NOW);
    assert!((found.pace_per_hour.unwrap() - 0.6).abs() < 1e-9);
    let delayed = observation(NOW - 2 * HOUR, 20.0, Some(NOW + 98 * HOUR), Some((5.0, NOW + 2 * HOUR)), Some(NOW));
    let policy = evaluate_policy(Provider::Claude, &[delayed], None, NOW, None);
    assert!((policy.estimated_weekly_used.unwrap() - (20.0 + (20.0 / 68.0) * 2.0)).abs() < 1e-9);
    assert!(policy.refresh_required);
}

#[test]
fn short_hard_brake_is_final_even_when_weekly_is_expired() {
    let stale_week = observation(NOW - 2 * HOUR, 90.0, Some(NOW - HOUR), Some((95.0, NOW + 2 * HOUR)), Some(NOW));
    let policy = evaluate_policy(Provider::Codex, &[stale_week], None, NOW, None);
    assert_eq!(policy.mode, Mode::Austere);
    assert_eq!(policy.reason, "short_window_hard_brake");
}

#[test]
fn short_projection_at_84_percent_brakes_one_level() {
    let item = observation(NOW, 0.2, Some(NOW + 20 * HOUR), Some((84.0, NOW + 4 * HOUR)), None);
    let policy = evaluate_policy(Provider::Claude, &[item], None, NOW, None);
    assert_eq!(policy.short_projected_used, Some(100.0));
    assert_eq!(policy.mode, Mode::Rich);
    assert_eq!(policy.reason, "short_window_pressure_brake");
}

#[test]
fn measured_short_usage_at_85_brakes_without_projection_ceiling() {
    let item = observation(NOW, 0.2, Some(NOW + 20 * HOUR), Some((85.0, NOW + 12 * MINUTE)), None);
    let policy = evaluate_policy(Provider::Claude, &[item], None, NOW, None);
    assert!(policy.short_projected_used.unwrap() < 100.0);
    assert_eq!(policy.mode, Mode::Rich);
    assert_eq!(policy.reason, "short_window_pressure_brake");
}

#[test]
fn stale_but_active_85_percent_short_reading_deducts_from_lean() {
    let item = observation(NOW - 2 * HOUR, 42.0, Some(NOW + 100 * HOUR), Some((85.0, NOW + 3 * HOUR)), Some(NOW));
    assert_eq!(weekly_metrics(std::slice::from_ref(&item), NOW).raw_mode, Mode::Lean);
    let policy = evaluate_policy(Provider::Claude, &[item], None, NOW, None);
    assert_eq!(policy.mode, Mode::Austere);
    assert_eq!(policy.reason, "short_window_pressure_brake");
    assert!(policy.refresh_required);
    assert_eq!(policy.freshness, "short_stale_ask_refresh");
}

#[test]
fn missing_and_expired_windows_are_conservative() {
    assert_eq!(evaluate_policy(Provider::Claude, &[], None, NOW, None).mode, Mode::Lean);
    let expired = observation(NOW - 6 * HOUR, 40.0, Some(NOW - HOUR), Some((40.0, NOW - 2 * HOUR)), Some(NOW));
    let policy = evaluate_policy(Provider::Claude, &[expired], None, NOW, None);
    assert_eq!(policy.mode, Mode::Lean);
    assert!(policy.refresh_required);
}

#[test]
fn upward_hysteresis_needs_two_distinct_observations_and_cooldown() {
    let deadline = NOW + 100 * HOUR;
    let state = GovernorState {
        window_reset_at: Some(deadline),
        base_mode: Some(Mode::Normal),
        last_change_at: Some(NOW - 2 * HOUR),
        ..Default::default()
    };
    let rich = metrics("weekly_headroom_high", deadline, 1.5, Mode::Rich, None);
    let first = advance_state(Some(&state), &rich, NOW, NOW, false);
    assert_eq!(first.base_mode, Some(Mode::Normal));
    assert_eq!(first.candidate_mode, Some(Mode::Rich));
    let replay = advance_state(Some(&first), &rich, NOW + HOUR, NOW, false);
    assert_eq!(replay.base_mode, Some(Mode::Normal));
    let second_at = NOW + HOUR;
    let promoted = advance_state(Some(&first), &rich, second_at, second_at, false);
    assert_eq!(promoted.base_mode, Some(Mode::Rich));
}

#[test]
fn aged_import_cannot_confirm_upgrade_but_can_downgrade() {
    let deadline = NOW + 100 * HOUR;
    let stale_high = metrics("weekly_headroom_high", deadline, 1.5, Mode::Rich, Some(2 * HOUR));
    let candidate = GovernorState {
        window_reset_at: Some(deadline),
        base_mode: Some(Mode::Normal),
        last_change_at: Some(NOW - 4 * HOUR),
        candidate_mode: Some(Mode::Rich),
        candidate_first_at: Some(NOW - 2 * HOUR),
        candidate_last_at: Some(NOW - 2 * HOUR),
        ..Default::default()
    };
    let held = advance_state(Some(&candidate), &stale_high, NOW, NOW - 2 * HOUR, false);
    assert_eq!(held.base_mode, Some(Mode::Normal));
    assert_eq!(held.candidate_mode, None);

    let stale_low = metrics("weekly_headroom_tight", deadline, 0.5, Mode::Lean, Some(2 * HOUR));
    let rich = GovernorState {
        window_reset_at: Some(deadline),
        base_mode: Some(Mode::Rich),
        last_change_at: Some(NOW - 4 * HOUR),
        ..Default::default()
    };
    let lowered = advance_state(Some(&rich), &stale_low, NOW, NOW - 2 * HOUR, false);
    assert_eq!(lowered.base_mode, Some(Mode::Lean));
}

#[test]
fn stale_gap_clears_candidate_before_fresh_recovery() {
    let deadline = NOW + 100 * HOUR;
    let state = GovernorState {
        window_reset_at: Some(deadline),
        base_mode: Some(Mode::Normal),
        last_change_at: Some(NOW - 4 * HOUR),
        candidate_mode: Some(Mode::Rich),
        candidate_first_at: Some(NOW - 2 * HOUR),
        candidate_last_at: Some(NOW - 2 * HOUR),
        ..Default::default()
    };
    let unavailable =
        advance_state(Some(&state), &WeeklyMetrics::unavailable("weekly_observation_stale"), NOW, NOW, false);
    assert_eq!(unavailable.candidate_mode, None);
    let fresh = metrics("weekly_headroom_high", deadline, 1.5, Mode::Rich, Some(0));
    let recovered = advance_state(Some(&state), &fresh, NOW, NOW, true);
    assert_eq!(recovered.base_mode, Some(Mode::Normal));
    assert_eq!(recovered.candidate_mode, Some(Mode::Rich));
    assert_eq!(recovered.candidate_first_at, Some(NOW));

    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    let prior_at = NOW - 3 * HOUR;
    record(&db, "claude", prior_at, 20.0, deadline, None, prior_at);
    sql(&db)
        .execute(
            "UPDATE governor_state SET base_mode='Normal', last_change_at=?, candidate_mode='Rich', \
             candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
            [(NOW - 5 * HOUR).iso(), (NOW - 2 * HOUR).iso(), (NOW - 2 * HOUR).iso()],
        )
        .unwrap();
    record(&db, "claude", NOW, 21.0, deadline, None, NOW);
    let (base, candidate, first): (String, String, String) = sql(&db)
        .query_row(
            "SELECT base_mode, candidate_mode, candidate_first_at FROM governor_state WHERE provider='claude'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((base.as_str(), candidate.as_str()), ("Normal", "Rich"));
    assert_eq!(demon_mode::ts::parse_text(&first), Ok(NOW));
}

#[test]
fn new_reset_window_replaces_transition_candidate() {
    let temp = TempDir::new();
    let db = temp.db("state.sqlite3");
    record(&db, "claude", NOW, 20.0, NOW + 100 * HOUR, None, NOW);
    sql(&db)
        .execute(
            "UPDATE governor_state SET candidate_mode='Rich', candidate_first_at=?, candidate_last_at=? WHERE provider='claude'",
            [NOW.iso(), NOW.iso()],
        )
        .unwrap();
    let later = NOW + HOUR;
    let new_reset = later + 168 * HOUR;
    record(&db, "claude", later, 1.0, new_reset, None, later);
    let (window, candidate, first): (String, Option<String>, Option<String>) = sql(&db)
        .query_row(
            "SELECT window_reset_at, candidate_mode, candidate_first_at FROM governor_state WHERE provider='claude'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(demon_mode::ts::parse_text(&window), Ok(new_reset));
    assert_eq!((candidate, first), (None, None));
}
