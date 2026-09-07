//! Tests for the Codex adapter's two readings and the label rule they share.
//!
//! Codex is read twice over, and the two paths use different field names for the same
//! quantities. The live `wham/usage` reply measures a window in **seconds**
//! (`limit_window_seconds`, `reset_at`, `reset_after_seconds`); the rollout-log snapshot
//! measures it in **minutes** (`window_minutes`, `resets_at`, `resets_in_seconds`).
//! Mixing the two up turns a 5 hour window into a 5 minute one, so each set is tested
//! against the other's field names to prove they are not interchangeable.

use super::*;
use serde_json::json;

/// Tolerance for assertions on a reset derived from `now_ms()` plus an offset.
const SLACK_MS: u64 = 60_000;

fn labels(ws: &[LimitWindow]) -> Vec<&str> {
    ws.iter().map(|w| w.label.as_str()).collect()
}

fn ids(ws: &[LimitWindow]) -> Vec<&str> {
    ws.iter().map(|w| w.id.as_str()).collect()
}

#[test]
fn label_is_derived_from_the_window_length() {
    assert_eq!(label_for(Some(5.0), "primary"), "5m limit");
    assert_eq!(label_for(Some(59.0), "primary"), "59m limit");
    assert_eq!(label_for(Some(60.0), "primary"), "1h limit");
    assert_eq!(label_for(Some(300.0), "primary"), "5h limit");
    assert_eq!(label_for(Some(1439.0), "primary"), "23h limit");
    assert_eq!(label_for(Some(60.0 * 24.0 * 7.0), "secondary"), "Weekly limit");
    assert_eq!(label_for(Some(60.0 * 24.0 * 30.0), "secondary"), "Monthly limit");
    // A length with no special name still reads as a duration.
    assert_eq!(label_for(Some(60.0 * 24.0 * 3.0), "secondary"), "3d limit");
}

#[test]
fn label_falls_back_to_the_window_position_when_the_length_is_unknown() {
    // A free plan has been seen reporting a 30 day primary window, so the position is only
    // a fallback: it is never allowed to override a length that was actually reported.
    assert_eq!(label_for(None, "primary"), "Current session");
    assert_eq!(label_for(None, "secondary"), "Longer window");
    assert_eq!(label_for(Some(0.0), "primary"), "Current session");
    assert_eq!(label_for(Some(-5.0), "secondary"), "Longer window");
}

// ---------------- The live usage endpoint ----------------

#[test]
fn usage_reply_yields_both_windows() {
    let v = json!({
        "rate_limit": {
            "primary_window": {
                "used_percent": 40.0,
                "limit_window_seconds": 5.0 * 3600.0,
                "reset_at": 1_893_456_000.0
            },
            "secondary_window": {
                "used_percent": 12.5,
                "limit_window_seconds": 7.0 * 24.0 * 3600.0,
                "reset_at": 1_893_456_000.0
            }
        }
    });
    let ws = windows_from_usage(&v);
    assert_eq!(ids(&ws), ["primary", "secondary"]);
    assert_eq!(labels(&ws), ["5h limit", "Weekly limit"]);
    assert!((ws[0].used - 0.40).abs() < 1e-9);
    assert!((ws[1].used - 0.125).abs() < 1e-9);
    // reset_at is epoch seconds and must be scaled to milliseconds.
    assert_eq!(ws[0].resets_at, Some(1_893_456_000_000));
}

#[test]
fn usage_reply_accepts_a_relative_reset() {
    let v = json!({
        "rate_limit": {
            "primary_window": { "used_percent": 1.0, "reset_after_seconds": 3600.0 }
        }
    });
    let before = now_ms();
    let ws = windows_from_usage(&v);
    let resets = ws[0].resets_at.expect("a relative reset must still produce a time");
    assert!(
        resets >= before + 3_600_000 && resets <= before + 3_600_000 + SLACK_MS,
        "expected roughly one hour from now, got {resets}"
    );
}

#[test]
fn usage_reply_prefers_the_absolute_reset_over_the_relative_one() {
    let v = json!({
        "rate_limit": {
            "primary_window": {
                "used_percent": 1.0,
                "reset_at": 1_893_456_000.0,
                "reset_after_seconds": 3600.0
            }
        }
    });
    assert_eq!(windows_from_usage(&v)[0].resets_at, Some(1_893_456_000_000));
}

#[test]
fn usage_reply_ignores_the_rollout_field_names() {
    // window_minutes/resets_at belong to the rollout snapshot. If this parser started
    // honouring them, a 300 minute window would be read as a 300 second one.
    let v = json!({
        "rate_limit": {
            "primary_window": { "used_percent": 10.0, "window_minutes": 300.0, "resets_at": 1_893_456_000.0 }
        }
    });
    let ws = windows_from_usage(&v);
    assert_eq!(labels(&ws), ["Current session"]);
    assert_eq!(ws[0].resets_at, None);
}

#[test]
fn usage_reply_skips_windows_that_report_no_percentage() {
    let v = json!({
        "rate_limit": {
            "primary_window": { "limit_window_seconds": 18000.0 },
            "secondary_window": { "used_percent": 5.0 }
        }
    });
    assert_eq!(ids(&windows_from_usage(&v)), ["secondary"]);
}

#[test]
fn usage_reply_clamps_percentages() {
    let v = json!({
        "rate_limit": {
            "primary_window": { "used_percent": 130.0 },
            "secondary_window": { "used_percent": -4.0 }
        }
    });
    let ws = windows_from_usage(&v);
    assert_eq!(ws[0].used, 1.0);
    assert_eq!(ws[1].used, 0.0);
}

#[test]
fn usage_reply_survives_shapes_it_has_never_seen() {
    for v in [
        json!({}),
        json!(null),
        json!({ "rate_limit": "not an object" }),
        json!({ "rate_limit": { "primary_window": 7 } }),
        json!({ "rate_limit": { "primary_window": { "used_percent": "40" } } }),
    ] {
        assert!(windows_from_usage(&v).is_empty(), "unexpected windows for {v}");
    }
}

// ---------------- The rollout snapshot fallback ----------------

fn rollout_line(ts: &str, used: f64) -> String {
    json!({
        "timestamp": ts,
        "rate_limits": {
            "primary": { "used_percent": used, "window_minutes": 300.0, "resets_in_seconds": 3600.0 },
            "secondary": { "used_percent": 5.0, "window_minutes": 10080.0, "resets_at": 1_893_456_000.0 },
            "plan_type": "pro"
        }
    })
    .to_string()
}

#[test]
fn rollout_snapshot_reads_the_last_recorded_limits() {
    let text = format!(
        "{}\n{}\n{}\n",
        rollout_line("2026-09-07T10:00:00Z", 10.0),
        r#"{"type":"message","content":"nothing to do with limits"}"#,
        rollout_line("2026-09-07T12:00:00Z", 70.0),
    );
    let (ws, recorded, plan) = snapshot_from_rollout(&text).expect("a snapshot must be found");
    assert_eq!(ids(&ws), ["primary", "secondary"]);
    // The newest line wins, so the reading is 70 % and not the earlier 10 %.
    assert!((ws[0].used - 0.70).abs() < 1e-9);
    assert_eq!(labels(&ws), ["5h limit", "Weekly limit"]);
    assert_eq!(plan.as_deref(), Some("pro"));
    // Derived rather than hand-computed: an epoch constant typed by hand is its own bug.
    let expected = chrono::DateTime::parse_from_rfc3339("2026-09-07T12:00:00Z")
        .unwrap()
        .timestamp_millis() as u64;
    assert_eq!(recorded, Some(expected));
}

#[test]
fn rollout_snapshot_accepts_limits_nested_under_payload() {
    let line = json!({
        "timestamp": "2026-09-07T12:00:00Z",
        "payload": { "rate_limits": { "primary": { "used_percent": 25.0, "window_minutes": 300.0 } } }
    })
    .to_string();
    let (ws, _, _) = snapshot_from_rollout(&line).expect("payload-nested limits must be read");
    assert_eq!(ids(&ws), ["primary"]);
    assert!((ws[0].used - 0.25).abs() < 1e-9);
}

#[test]
fn rollout_snapshot_skips_unusable_lines_and_keeps_looking() {
    // A truncated tail, a line that merely mentions rate_limits, and a snapshot with no
    // usable window must all be stepped over rather than ending the search.
    let text = format!(
        "{}\n{}\n{}\n{}",
        rollout_line("2026-09-07T09:00:00Z", 33.0),
        r#"{"rate_limits":{}}"#,
        r#"{"rate_limits":{"primary":{"window_minutes":300.0}}}"#,
        r#"{"timestamp":"2026-09-07T13:00:00Z","rate_limits":{"pri"#,
    );
    let (ws, _, _) = snapshot_from_rollout(&text).expect("must fall back to the older line");
    assert!((ws[0].used - 0.33).abs() < 1e-9);
}

#[test]
fn rollout_snapshot_accepts_a_relative_reset() {
    let line = rollout_line("2026-09-07T12:00:00Z", 10.0);
    let before = now_ms();
    let (ws, _, _) = snapshot_from_rollout(&line).unwrap();
    let resets = ws[0].resets_at.expect("resets_in_seconds must produce a time");
    assert!(
        resets >= before + 3_600_000 && resets <= before + 3_600_000 + SLACK_MS,
        "expected roughly one hour from now, got {resets}"
    );
    // The secondary window used the absolute form.
    assert_eq!(ws[1].resets_at, Some(1_893_456_000_000));
}

#[test]
fn rollout_snapshot_tolerates_a_missing_timestamp_and_plan() {
    let line = r#"{"rate_limits":{"primary":{"used_percent":50.0,"window_minutes":300.0}}}"#;
    let (ws, recorded, plan) = snapshot_from_rollout(line).unwrap();
    assert_eq!(ws.len(), 1);
    // The snapshot is still usable; the caller marks it stale by its own timestamp, and a
    // missing one means it cannot claim an age rather than claiming a wrong one.
    assert_eq!(recorded, None);
    assert_eq!(plan, None);
}

#[test]
fn rollout_snapshot_returns_nothing_when_there_is_nothing_to_read() {
    assert!(snapshot_from_rollout("").is_none());
    assert!(snapshot_from_rollout("not json at all\nnor this\n").is_none());
    assert!(snapshot_from_rollout(r#"{"type":"message"}"#).is_none());
    // Mentions rate_limits but carries no usable window.
    assert!(snapshot_from_rollout(r#"{"rate_limits":{"plan_type":"pro"}}"#).is_none());
}

#[test]
fn jwt_claims_reads_the_payload_without_verifying_it() {
    // {"a":1} base64url-encoded, unpadded, as the CLIs write it.
    let token = "header.eyJhIjoxfQ.signature";
    assert_eq!(jwt_claims(token), Some(json!({ "a": 1 })));
    // Nothing here is trusted, so malformed input must return None rather than panic.
    assert_eq!(jwt_claims(""), None);
    assert_eq!(jwt_claims("only-one-segment"), None);
    assert_eq!(jwt_claims("header.!!!not-base64!!!.sig"), None);
    assert_eq!(jwt_claims("header.aGVsbG8.sig"), None); // valid base64, not JSON
}
