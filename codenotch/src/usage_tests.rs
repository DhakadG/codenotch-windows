//! Tests for the Claude usage adapter's pure parsing and back-off logic.
//!
//! Everything here works on `serde_json::Value` fixtures rather than live HTTP, so the
//! suite runs offline and deterministically. The fixtures are shaped after the documented
//! reply: `{ limits:[{kind,percent,resets_at}], five_hour:{utilization,resets_at}, seven_day:{...} }`.

use super::*;
use serde_json::json;

/// 2030-01-01T00:00:00Z. A fixed future date, so a fixture never goes stale against the wall clock.
const RESET_ISO: &str = "2030-01-01T00:00:00Z";
const RESET_MS: u64 = 1_893_456_000_000;

fn ids(ws: &[LimitWindow]) -> Vec<&str> {
    ws.iter().map(|w| w.id.as_str()).collect()
}

#[test]
fn parse_reset_accepts_rfc3339_and_rejects_anything_else() {
    assert_eq!(parse_reset(&json!(RESET_ISO)), Some(RESET_MS));
    // An offset is normalised to UTC rather than rejected.
    assert_eq!(
        parse_reset(&json!("2029-12-31T19:00:00-05:00")),
        Some(RESET_MS)
    );
    assert_eq!(parse_reset(&json!("not a date")), None);
    assert_eq!(parse_reset(&json!(1_893_456_000)), None);
    assert_eq!(parse_reset(&json!(null)), None);
}

#[test]
fn parse_reset_clamps_pre_epoch_dates_to_zero() {
    // A negative epoch cast to u64 would wrap to an enormous number and render as a reset
    // date far in the future, so it is clamped to zero instead.
    assert_eq!(parse_reset(&json!("1960-01-01T00:00:00Z")), Some(0));
}

#[test]
fn label_for_maps_known_kinds_and_humanises_unknown_ones() {
    assert_eq!(label_for("session"), "Current session");
    assert_eq!(label_for("seven_day"), "Weekly (all models)");
    assert_eq!(label_for("weekly_all"), "Weekly (all models)");
    assert_eq!(label_for("seven_day_opus"), "Weekly (Opus)");
    assert_eq!(label_for("weekly_opus"), "Weekly (Opus)");
    assert_eq!(label_for("weekly_scoped"), "Weekly (model-scoped)");
    // Forward compatibility: a kind this build has never heard of still gets a readable label.
    assert_eq!(label_for("monthly_sonnet"), "Monthly sonnet");
    assert_eq!(label_for(""), "");
}

#[test]
fn parse_response_reads_the_limits_array() {
    let v = json!({
        "limits": [
            { "kind": "session", "percent": 42.0, "resets_at": RESET_ISO },
            { "kind": "weekly_all", "percent": 7.5, "resets_at": RESET_ISO }
        ]
    });
    let ws = parse_response(&v);
    assert_eq!(ids(&ws), ["session", "weekly_all"]);
    assert!((ws[0].used - 0.42).abs() < 1e-9);
    assert!((ws[1].used - 0.075).abs() < 1e-9);
    assert_eq!(ws[0].resets_at, Some(RESET_MS));
    assert_eq!(ws[0].label, "Current session");
}

#[test]
fn parse_response_drops_windows_without_a_reset_time() {
    // Upstream rule: a window with no reset time is not shown at all. A bar with no
    // horizon cannot say when it clears, which reads as more alarming than it is.
    let v = json!({
        "limits": [
            { "kind": "session", "percent": 42.0 },
            { "kind": "weekly_all", "percent": 7.5, "resets_at": "garbage" }
        ]
    });
    assert!(parse_response(&v).is_empty());
}

#[test]
fn parse_response_skips_entries_missing_kind_or_percent() {
    let v = json!({
        "limits": [
            { "percent": 42.0, "resets_at": RESET_ISO },
            { "kind": "session", "resets_at": RESET_ISO },
            { "kind": "weekly_all", "percent": "7.5", "resets_at": RESET_ISO }
        ]
    });
    assert!(parse_response(&v).is_empty());
}

#[test]
fn parse_response_clamps_percentages_into_range() {
    let v = json!({
        "limits": [
            { "kind": "session", "percent": 140.0, "resets_at": RESET_ISO },
            { "kind": "weekly_all", "percent": -5.0, "resets_at": RESET_ISO }
        ]
    });
    let ws = parse_response(&v);
    assert_eq!(ws[0].used, 1.0);
    assert_eq!(ws[1].used, 0.0);
}

#[test]
fn parse_response_merges_the_named_fallback_fields() {
    // A window that has just rolled over disappears from `limits` while the named field
    // survives. Without the merge the cell would lose a ring at every reset.
    let v = json!({
        "limits": [],
        "five_hour": { "utilization": 12.0, "resets_at": RESET_ISO },
        "seven_day": { "utilization": 34.0, "resets_at": RESET_ISO }
    });
    let ws = parse_response(&v);
    assert_eq!(ids(&ws), ["session", "seven_day"]);
    assert!((ws[0].used - 0.12).abs() < 1e-9);
    assert!((ws[1].used - 0.34).abs() < 1e-9);
}

#[test]
fn parse_response_dedupes_a_window_present_in_both_shapes() {
    // One case per dedupe rule: id alias, matching reset time with a near-identical
    // percentage, and identical label.
    let by_alias = json!({
        "limits": [{ "kind": "weekly_all", "percent": 34.0, "resets_at": RESET_ISO }],
        "seven_day": { "utilization": 99.0, "resets_at": "2031-01-01T00:00:00Z" }
    });
    assert_eq!(ids(&parse_response(&by_alias)), ["weekly_all"]);

    let by_reset_and_percent = json!({
        "limits": [{ "kind": "weekly_scoped", "percent": 34.0, "resets_at": RESET_ISO }],
        "seven_day": { "utilization": 34.2, "resets_at": RESET_ISO }
    });
    assert_eq!(ids(&parse_response(&by_reset_and_percent)), ["weekly_scoped"]);

    let by_label = json!({
        "limits": [{ "kind": "five_hour", "percent": 10.0, "resets_at": RESET_ISO }],
        "five_hour": { "utilization": 88.0, "resets_at": "2031-01-01T00:00:00Z" }
    });
    // five_hour and session share the "Current session" label, so the fallback is dropped.
    assert_eq!(ids(&parse_response(&by_label)), ["five_hour"]);
}

#[test]
fn parse_response_keeps_a_genuinely_different_window_from_the_fallback() {
    // The opposite failure: dedupe must not swallow a window that really is new.
    let v = json!({
        "limits": [{ "kind": "weekly_opus", "percent": 10.0, "resets_at": RESET_ISO }],
        "seven_day": { "utilization": 80.0, "resets_at": "2031-01-01T00:00:00Z" }
    });
    assert_eq!(ids(&parse_response(&v)), ["weekly_opus", "seven_day"]);
}

#[test]
fn parse_response_puts_the_session_window_first() {
    let v = json!({
        "limits": [
            { "kind": "weekly_all", "percent": 7.5, "resets_at": RESET_ISO },
            { "kind": "weekly_opus", "percent": 1.0, "resets_at": RESET_ISO },
            { "kind": "session", "percent": 42.0, "resets_at": RESET_ISO }
        ]
    });
    assert_eq!(parse_response(&v)[0].id, "session");
}

#[test]
fn parse_response_survives_shapes_it_has_never_seen() {
    // The parser must never panic on a payload change. An empty reading is handled
    // upstream by keeping the previous one and marking it stale.
    for v in [
        json!({}),
        json!(null),
        json!([]),
        json!({ "limits": "not an array" }),
        json!({ "limits": [null, 7, "x"] }),
        json!({ "five_hour": "not an object" }),
        json!({ "five_hour": { "utilization": "12" } }),
    ] {
        assert!(parse_response(&v).is_empty(), "unexpected windows for {v}");
    }
}

#[test]
fn model_scoped_weekly_windows_are_read_when_the_plan_has_them() {
    // Null on Pro, populated on Max and Team. Reading them costs nothing on the plans that
    // leave them null, and a Max account was previously shown none of them.
    let v = json!({
        "limits": [],
        "seven_day_opus": { "utilization": 22.0, "resets_at": RESET_ISO },
        "seven_day_sonnet": { "utilization": 8.0, "resets_at": RESET_ISO }
    });
    let ws = parse_response(&v);
    assert_eq!(ids(&ws), ["seven_day_opus", "seven_day_sonnet"]);
    assert_eq!(ws[0].label, "Weekly (Opus)");
    assert_eq!(ws[1].label, "Weekly (Sonnet)");
    assert!((ws[0].used - 0.22).abs() < 1e-9);
}

#[test]
fn extra_usage_is_shown_only_when_it_is_switched_on() {
    let off = json!({
        "limits": [],
        "extra_usage": { "is_enabled": false, "monthly_limit": null, "used_credits": null }
    });
    assert!(parse_response(&off).is_empty());

    // A Pro account with the feature never enabled: every field null, still nothing shown.
    let never = json!({ "limits": [], "extra_usage": { "is_enabled": false, "user_disabled": true } });
    assert!(parse_response(&never).is_empty());
}

#[test]
fn extra_usage_is_a_ratio_of_money_not_a_utilization() {
    // 1500 cents of a 5000 cent cap. `utilization` is deliberately ignored: it is null
    // until the first spend of a cycle, so a bar keyed on it disappears every month start.
    let v = json!({
        "limits": [],
        "extra_usage": {
            "is_enabled": true,
            "monthly_limit": 5000.0,
            "used_credits": 1500.0,
            "utilization": null
        }
    });
    let ws = parse_response(&v);
    assert_eq!(ids(&ws), ["extra_usage"]);
    assert_eq!(ws[0].label, "Extra usage");
    assert!((ws[0].used - 0.3).abs() < 1e-9);
    assert_eq!(ws[0].count, None);
}

#[test]
fn uncapped_extra_usage_shows_an_amount_rather_than_a_share() {
    // Enabled with no ceiling: there is no denominator, so there is no honest percentage.
    // Upstream's rule - a count, marked derived, never an invented share.
    let v = json!({
        "limits": [],
        "extra_usage": { "is_enabled": true, "monthly_limit": null, "used_credits": 734.0 }
    });
    let ws = parse_response(&v);
    assert_eq!(ws[0].count, Some(7)); // 734 cents rounds to 7 dollars
    assert!(ws[0].derived);
    assert_eq!(ws[0].used, 0.0);
}

// ---------------- The request budget ----------------

#[test]
fn the_request_log_forgets_anything_older_than_an_hour() {
    let now = 10 * HOUR_MS;
    let mut log = vec![
        now - HOUR_MS - 1, // just outside
        now - HOUR_MS,     // exactly an hour old, also outside
        now - HOUR_MS + 1, // just inside
        now,
    ];
    assert_eq!(prune_request_log(&mut log, now), 2);
    assert_eq!(log, vec![now - HOUR_MS + 1, now]);
}

#[test]
fn the_budget_allows_normal_polling() {
    // The five minute floor produces twelve requests an hour, comfortably under the cap,
    // so ordinary operation must never be held back by this guardrail.
    let now = 10 * HOUR_MS;
    let mut log: Vec<u64> = (0..12).map(|i| now - i * 5 * 60 * 1000).collect();
    assert_eq!(budget_check(&mut log, now), Ok(()));
}

#[test]
fn the_budget_stops_a_burst_and_says_when_it_lifts() {
    // The failure this exists for: many short-lived app starts in quick succession, each
    // spending a request, which is how this endpoint was tripped in practice.
    let now = 10 * HOUR_MS;
    let mut log: Vec<u64> = (0..MAX_REQUESTS_PER_HOUR as u64).map(|i| now - i * 1000).collect();
    let Err(wait) = budget_check(&mut log, now) else {
        panic!("a full hour's budget spent in 20 seconds should have been refused");
    };
    // The oldest entry is 19 s old, so the window frees up just under an hour from now.
    assert!(
        (3570..=3600).contains(&wait),
        "expected roughly an hour, got {wait}s"
    );
}

#[test]
fn the_budget_frees_up_as_old_requests_age_out() {
    let now = 10 * HOUR_MS;
    // Full, but every entry is nearly an hour old.
    let mut log: Vec<u64> =
        (0..MAX_REQUESTS_PER_HOUR as u64).map(|i| now - HOUR_MS + 10_000 + i * 100).collect();
    let Err(wait) = budget_check(&mut log, now) else {
        panic!("still full at this instant");
    };
    assert!(wait <= 10, "the oldest entry expires within 10s, got {wait}s");

    // Ten seconds later the oldest has aged out and a request is allowed again.
    assert_eq!(budget_check(&mut log, now + 10_001), Ok(()));
}

#[test]
fn an_empty_budget_never_reports_a_zero_wait() {
    // A zero would busy-loop the caller, which is the opposite of what a guardrail is for.
    let now = 10 * HOUR_MS;
    let mut log: Vec<u64> = vec![now; MAX_REQUESTS_PER_HOUR];
    match budget_check(&mut log, now + HOUR_MS - 1) {
        Err(wait) => assert!(wait >= 1),
        Ok(()) => panic!("still inside the window"),
    }
}

#[test]
fn backoff_doubles_then_stops_at_the_cap() {
    assert_eq!(backoff_secs(0, 0), 60);
    assert_eq!(backoff_secs(1, 0), 120);
    assert_eq!(backoff_secs(2, 0), 240);
    assert_eq!(backoff_secs(3, 0), 480);
    // 60 * 2^4 = 960, above the 15 minute cap.
    assert_eq!(backoff_secs(4, 0), BACKOFF_CAP_SECS);
    assert_eq!(backoff_secs(5, 0), BACKOFF_CAP_SECS);
    // The shift is saturated, so a runaway counter cannot wrap around to a short wait.
    assert_eq!(backoff_secs(u32::MAX, 0), BACKOFF_CAP_SECS);
}

#[test]
fn retry_after_raises_the_backoff_but_never_lowers_it() {
    assert_eq!(backoff_secs(0, 30), 60);
    assert_eq!(backoff_secs(0, 300), 300);
    // Deliberate: a Retry-After above the local cap is honoured. The server knows more
    // about the account's state than the cap does, and ignoring it earns a harder limit.
    assert_eq!(backoff_secs(4, 3600), 3600);
}
