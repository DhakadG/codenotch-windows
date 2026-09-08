//! Tests for the Antigravity adapter's pure parsing helpers.
//!
//! Antigravity is the one provider the original author could not verify against a real
//! install, so the parsing rules are pinned here rather than left to a live reading:
//! the bridge reports what **remains** while the notch shows what is **used**, and the
//! two names in a bucket are not interchangeable.

use super::*;
use serde_json::json;

const RESET_ISO: &str = "2030-01-01T00:00:00Z";
const RESET_MS: u64 = 1_893_456_000_000;

#[test]
fn flag_value_reads_the_argument_after_a_flag() {
    let line = r#"language_server.exe --port 51234 --token "abc123" --verbose"#;
    assert_eq!(flag_value(line, "--port").as_deref(), Some("51234"));
    // Quotes around a value are stripped; the process list quotes some arguments and not others.
    assert_eq!(flag_value(line, "--token").as_deref(), Some("abc123"));
    assert_eq!(flag_value(line, "--missing"), None);
    // A flag at the very end has no value to take, and must not read past the arguments.
    assert_eq!(flag_value(line, "--verbose"), None);
    assert_eq!(flag_value("", "--port"), None);
}

#[test]
fn a_staging_language_server_is_recognised() {
    // An Antigravity install runs a production client and a `daily-` one at the same time.
    // They serve different numbers, so picking between them cannot be left to process order.
    let daily = "1234\tls.exe --csrf_token x --cloud_code_endpoint https://daily-cloudcode-pa.googleapis.com --subclient_type ide";
    let prod = "5678\tls.exe --csrf_token x --cloud_code_endpoint https://cloudcode-pa.googleapis.com --subclient_type ide";
    assert!(is_staging_endpoint(daily));
    assert!(!is_staging_endpoint(prod));

    assert!(is_staging_endpoint("ls.exe --cloud_code_endpoint https://staging-cloudcode-pa.googleapis.com"));
    assert!(is_staging_endpoint("ls.exe --cloud_code_endpoint https://autopush-cloudcode-pa.googleapis.com"));

    // Only the host label counts. A production host whose name merely contains "daily"
    // later on is not staging, and a missing flag is not evidence of anything.
    assert!(!is_staging_endpoint("ls.exe --cloud_code_endpoint https://cloudcode-pa-daily.googleapis.com"));
    assert!(!is_staging_endpoint("ls.exe --csrf_token x"));
    assert!(!is_staging_endpoint(""));
}

#[test]
fn bucket_labels_are_unique_within_a_group() {
    // The regression this exists for: every bucket took its group's name, so a live install
    // showed "Gemini Models" twice and "Claude and GPT models" twice on one card.
    assert_eq!(bucket_label(Some("Gemini Models"), Some("Five Hour Limit Remaining"), Some("5h")), "Gemini Models (5h)");
    assert_eq!(bucket_label(Some("Gemini Models"), Some("Weekly Limit Remaining"), Some("weekly")), "Gemini Models (weekly)");
    assert_eq!(
        bucket_label(Some("Claude and GPT models"), Some("Weekly Limit Remaining"), Some("weekly")),
        "Claude and GPT models (weekly)"
    );
}

#[test]
fn bucket_labels_degrade_in_a_documented_order() {
    // An unfamiliar window is passed through rather than dropped, so a new bucket type is
    // still told apart from its siblings.
    assert_eq!(bucket_label(Some("Gemini Models"), None, Some("monthly")), "Gemini Models (monthly)");
    // No window field: fall back to the bucket's own name, without the suffix they all share.
    assert_eq!(
        bucket_label(Some("Gemini Models"), Some("Five Hour Limit Remaining"), None),
        "Gemini Models (Five Hour Limit)"
    );
    // Nothing to qualify it with: the group name alone is still better than a placeholder.
    assert_eq!(bucket_label(Some("Gemini Models"), None, None), "Gemini Models");
    assert_eq!(bucket_label(Some("Gemini Models"), Some(""), Some("")), "Gemini Models");
    // No group either.
    assert_eq!(bucket_label(None, Some("Weekly Limit Remaining"), None), "Weekly Limit");
    assert_eq!(bucket_label(None, None, Some("5h")), "5h");
    assert_eq!(bucket_label(None, None, None), "Usage");
}

#[test]
fn a_real_bridge_reply_produces_four_distinctly_named_windows() {
    // Captured from a live language_server on 2026-09-07, with the fractions rounded.
    // This is the shape the original port could not verify for want of an install.
    let v = json!({
        "response": { "groups": [
            {
                "displayName": "Gemini Models",
                "buckets": [
                    { "bucketId": "gemini-weekly", "displayName": "Weekly Limit Remaining", "window": "weekly", "remainingFraction": 0.9999, "resetTime": "2026-09-14T15:48:44Z" },
                    { "bucketId": "gemini-5h", "displayName": "Five Hour Limit Remaining", "window": "5h", "remainingFraction": 0.9995, "resetTime": "2026-09-07T20:48:44Z" }
                ]
            },
            {
                "displayName": "Claude and GPT models",
                "buckets": [
                    { "bucketId": "3p-weekly", "displayName": "Weekly Limit Remaining", "window": "weekly", "remainingFraction": 1, "resetTime": "2026-09-14T15:53:00Z" },
                    { "bucketId": "3p-5h", "displayName": "Five Hour Limit Remaining", "window": "5h", "remainingFraction": 1, "resetTime": "2026-09-07T20:53:00Z" }
                ]
            }
        ]}
    });
    let ws = windows_from_bridge(&v);
    assert_eq!(
        ws.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
        ["gemini-weekly", "gemini-5h", "3p-weekly", "3p-5h"]
    );
    let labels: Vec<&str> = ws.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Gemini Models (weekly)",
            "Gemini Models (5h)",
            "Claude and GPT models (weekly)",
            "Claude and GPT models (5h)"
        ]
    );
    let mut unique = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), labels.len(), "two rings must never share a label");
    // Barely-used pools read as barely used, not as full.
    assert!((ws[0].used - 0.0001).abs() < 1e-6);
    assert_eq!(ws[2].used, 0.0);
}

#[test]
fn parse_iso_reads_a_reset_time() {
    assert_eq!(parse_iso(Some(&json!(RESET_ISO))), Some(RESET_MS));
    assert_eq!(parse_iso(Some(&json!("nonsense"))), None);
    assert_eq!(parse_iso(Some(&json!(1_893_456_000))), None);
    assert_eq!(parse_iso(None), None);
    assert_eq!(parse_iso(Some(&json!("1960-01-01T00:00:00Z"))), Some(0));
}

#[test]
fn b64_decode_accepts_both_alphabets_and_optional_padding() {
    assert_eq!(b64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
    assert_eq!(b64_decode("aGVsbG8").as_deref(), Some(&b"hello"[..]));
    // URL-safe alphabet, as JWT segments use.
    assert_eq!(b64_decode("Pz8_Pg").as_deref(), Some(&b"???>"[..]));
    assert_eq!(b64_decode("Pz8/Pg==").as_deref(), Some(&b"???>"[..]));
    // Line breaks and spaces are ignored, not treated as data.
    assert_eq!(b64_decode("aGVs\nbG8=").as_deref(), Some(&b"hello"[..]));
    assert_eq!(b64_decode("").as_deref(), Some(&b""[..]));
    // Anything outside both alphabets is a decode failure rather than silently dropped
    // bytes, because a half-decoded token would be sent as a credential.
    assert_eq!(b64_decode("not base64!"), None);
    assert_eq!(b64_decode("aGVsbG8\u{00e9}"), None);
}

#[test]
fn bridge_quota_is_inverted_from_remaining_to_used() {
    // The single most important rule in this file: the server says how much is left, the
    // ring draws how much is gone. Getting this backwards renders a full account as empty.
    let v = json!({
        "response": { "groups": [{
            "displayName": "Gemini",
            "buckets": [{ "bucketId": "gemini-pool", "remainingFraction": 0.25, "resetTime": RESET_ISO }]
        }]}
    });
    let ws = windows_from_bridge(&v);
    assert_eq!(ws.len(), 1);
    assert!((ws[0].used - 0.75).abs() < 1e-9);
    assert_eq!(ws[0].id, "gemini-pool");
    assert_eq!(ws[0].label, "Gemini");
    assert_eq!(ws[0].resets_at, Some(RESET_MS));
}

#[test]
fn a_full_and_an_empty_pool_both_read_correctly() {
    let pool = |remaining: f64| {
        json!({
            "response": { "groups": [{
                "displayName": "Gemini",
                "buckets": [{ "bucketId": "p", "remainingFraction": remaining }]
            }]}
        })
    };
    assert_eq!(windows_from_bridge(&pool(1.0))[0].used, 0.0);
    assert_eq!(windows_from_bridge(&pool(0.0))[0].used, 1.0);
}

#[test]
fn a_remaining_fraction_outside_the_unit_range_is_rejected_not_clamped() {
    // Deliberate: a fraction above 1 or below 0 means the wire format changed, and
    // clamping would draw a confident ring from a value nobody understands.
    for bad in [1.5, -0.1, 100.0] {
        let v = json!({
            "response": { "groups": [{
                "displayName": "Gemini",
                "buckets": [{ "bucketId": "p", "remainingFraction": bad }]
            }]}
        });
        assert!(
            windows_from_bridge(&v).is_empty(),
            "remainingFraction {bad} should have been rejected"
        );
    }
}

#[test]
fn the_bucket_identifies_the_window_and_both_names_label_it() {
    // The id and the label are not interchangeable: the id keys persistence and the label
    // is what the card shows, so a swap would relabel every ring after an update.
    let v = json!({
        "response": { "groups": [{
            "displayName": "Gemini",
            "buckets": [{ "bucketId": "fast", "displayName": "Fast requests", "remainingFraction": 0.5 }]
        }]}
    });
    let ws = windows_from_bridge(&v);
    assert_eq!(ws[0].id, "fast");
    assert_eq!(ws[0].label, "Gemini (Fast requests)");
}

#[test]
fn missing_names_fall_back_in_a_documented_order() {
    // id: bucketId, then the group name, then "quota".
    // label: the group name, then the bucket name, then "Usage".
    let no_bucket_id = json!({
        "response": { "groups": [{
            "displayName": "Gemini",
            "buckets": [{ "remainingFraction": 0.5 }]
        }]}
    });
    let ws = windows_from_bridge(&no_bucket_id);
    assert_eq!(ws[0].id, "Gemini");
    assert_eq!(ws[0].label, "Gemini");

    let no_names_at_all = json!({
        "response": { "groups": [{ "buckets": [{ "remainingFraction": 0.5 }] }] }
    });
    let ws = windows_from_bridge(&no_names_at_all);
    assert_eq!(ws[0].id, "quota");
    assert_eq!(ws[0].label, "Usage");

    let bucket_name_only = json!({
        "response": { "groups": [{
            "buckets": [{ "displayName": "Fast requests", "remainingFraction": 0.5 }]
        }]}
    });
    assert_eq!(windows_from_bridge(&bucket_name_only)[0].label, "Fast requests");
}

#[test]
fn every_group_and_bucket_is_read() {
    let v = json!({
        "response": { "groups": [
            {
                "displayName": "Gemini",
                "buckets": [
                    { "bucketId": "a", "remainingFraction": 0.9 },
                    { "bucketId": "b", "remainingFraction": 0.1 }
                ]
            },
            { "displayName": "Other", "buckets": [{ "bucketId": "c", "remainingFraction": 0.5 }] }
        ]}
    });
    let ws = windows_from_bridge(&v);
    assert_eq!(
        ws.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
}

#[test]
fn a_bucket_with_no_fraction_is_skipped_without_losing_its_siblings() {
    let v = json!({
        "response": { "groups": [{
            "displayName": "Gemini",
            "buckets": [
                { "bucketId": "a" },
                { "bucketId": "b", "remainingFraction": "0.5" },
                { "bucketId": "c", "remainingFraction": 0.5 }
            ]
        }]}
    });
    assert_eq!(
        windows_from_bridge(&v)
            .iter()
            .map(|w| w.id.as_str())
            .collect::<Vec<_>>(),
        ["c"]
    );
}

#[test]
fn a_window_without_a_reset_time_is_still_reported() {
    // Unlike Claude, Antigravity's pools are not always on a published schedule, so a
    // missing reset time is expected rather than a reason to hide the reading.
    let v = json!({
        "response": { "groups": [{
            "displayName": "Gemini",
            "buckets": [{ "bucketId": "p", "remainingFraction": 0.5 }]
        }]}
    });
    let ws = windows_from_bridge(&v);
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0].resets_at, None);
}

#[test]
fn bridge_parsing_survives_shapes_it_has_never_seen() {
    for v in [
        json!({}),
        json!(null),
        json!({ "response": null }),
        json!({ "response": { "groups": "not an array" } }),
        json!({ "response": { "groups": [null, 7, "x"] } }),
        json!({ "response": { "groups": [{ "buckets": "not an array" }] } }),
        json!({ "response": { "groups": [{ "buckets": [null] }] } }),
        json!({ "groups": [{ "buckets": [{ "remainingFraction": 0.5 }] }] }),
    ] {
        assert!(windows_from_bridge(&v).is_empty(), "unexpected windows for {v}");
    }
}

/// Which port is tried, and in what order.
///
/// The rule is the whole point of the fix: once a port has served the RPC, the other one must
/// not be contacted again. Poking a listener with a protocol it does not speak is what tore
/// down streams inside Antigravity itself.
fn probe_order(ports: &[u16], remembered: Option<u16>) -> Vec<u16> {
    remembered
        .filter(|p| ports.contains(p))
        .into_iter()
        .chain(ports.iter().copied().filter(|p| Some(*p) != remembered))
        .collect()
}

#[test]
fn the_known_good_port_is_tried_first_and_never_listed_twice() {
    let ports = vec![42100u16, 42101];
    // Nothing remembered yet: discovery order, both candidates present.
    assert_eq!(probe_order(&ports, None), vec![42100, 42101]);
    // Remembered: it leads, and appears exactly once.
    assert_eq!(probe_order(&ports, Some(42101)), vec![42101, 42100]);
    assert_eq!(probe_order(&ports, Some(42100)), vec![42100, 42101]);
    // A remembered port the server no longer listens on is dropped, not dialled: the language
    // server restarts on different ports, and chasing a dead one wastes the first attempt of
    // every poll.
    assert_eq!(probe_order(&ports, Some(9999)), vec![42100, 42101]);
    assert!(probe_order(&[], Some(42100)).is_empty());
}
