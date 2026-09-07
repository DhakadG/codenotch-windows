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
