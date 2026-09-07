//! Tests for the Cursor adapter's `usage-summary` parsing.
//!
//! Cursor borrows the editor's own session, so there is exactly one account and no
//! sign-in state to model. What varies is the plan: a paid plan meters included usage,
//! optionally API usage and optionally on-demand spend, while an unlimited or free plan
//! meters nothing at all and must say so rather than showing an empty ring.

use super::*;
use serde_json::json;

const CYCLE_END_ISO: &str = "2030-01-01T00:00:00Z";
const CYCLE_END_MS: u64 = 1_893_456_000_000;

fn ids(ws: &[LimitWindow]) -> Vec<&str> {
    ws.iter().map(|w| w.id.as_str()).collect()
}

/// Builds a JWT with the given payload. Only the payload segment is meaningful; the header
/// and signature are filler, because nothing here verifies a signature.
fn jwt_with(payload: &serde_json::Value) -> String {
    use std::fmt::Write as _;
    let raw = serde_json::to_vec(payload).unwrap();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::new();
    for chunk in raw.chunks(3) {
        let mut buf = [0u8; 3];
        buf[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes([0, buf[0], buf[1], buf[2]]);
        for i in 0..chunk.len() + 1 {
            let _ = write!(encoded, "{}", alphabet[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    format!("header.{encoded}.signature")
}

#[test]
fn the_account_id_is_read_from_the_tokens_sub_claim() {
    // The fallback that keeps free accounts working: Cursor writes
    // `cursorAuth/stripeMembershipAuthId` only for accounts with a Stripe membership, so on
    // a free plan the id has to come from the token itself.
    let token = jwt_with(&json!({ "sub": "google-oauth2|1234567890", "type": "session" }));
    assert_eq!(jwt_sub(&token).as_deref(), Some("google-oauth2|1234567890"));
}

#[test]
fn a_token_with_no_usable_sub_claim_yields_no_id() {
    // Each of these must return None rather than producing a malformed cookie, which would
    // be sent to Cursor and rejected in a way that looks like a sign-in problem.
    assert_eq!(jwt_sub(&jwt_with(&json!({ "type": "session" }))), None);
    assert_eq!(jwt_sub(&jwt_with(&json!({ "sub": "" }))), None);
    assert_eq!(jwt_sub(&jwt_with(&json!({ "sub": 12345 }))), None);
    assert_eq!(jwt_sub("not-a-jwt"), None);
    assert_eq!(jwt_sub(""), None);
    assert_eq!(jwt_sub("header.!!!.signature"), None);
    assert_eq!(jwt_sub("header.aGVsbG8.signature"), None); // valid base64, not JSON
}

#[test]
fn pct_scales_and_clamps() {
    assert_eq!(pct(Some(&json!(0.0))), Some(0.0));
    assert_eq!(pct(Some(&json!(50))), Some(0.5));
    assert_eq!(pct(Some(&json!(140.0))), Some(1.0));
    assert_eq!(pct(Some(&json!(-1.0))), Some(0.0));
    assert_eq!(pct(Some(&json!("50"))), None);
    assert_eq!(pct(Some(&json!(null))), None);
    assert_eq!(pct(None), None);
}

#[test]
fn parse_iso_reads_the_billing_cycle_end() {
    assert_eq!(parse_iso(Some(&json!(CYCLE_END_ISO))), Some(CYCLE_END_MS));
    assert_eq!(parse_iso(Some(&json!("nonsense"))), None);
    assert_eq!(parse_iso(None), None);
    // Pre-epoch clamps rather than wrapping to a far-future date.
    assert_eq!(parse_iso(Some(&json!("1960-01-01T00:00:00Z"))), Some(0));
}

#[test]
fn parse_summary_reads_a_full_paid_plan() {
    let v = json!({
        "billingCycleEnd": CYCLE_END_ISO,
        "individualUsage": {
            "plan": { "totalPercentUsed": 63.0, "apiPercentUsed": 12.0 },
            "onDemand": { "enabled": true, "limit": 50.0, "used": 20.0 }
        }
    });
    let (ws, note) = parse_summary(&v);
    assert_eq!(ids(&ws), ["included", "api", "on_demand"]);
    assert!((ws[0].used - 0.63).abs() < 1e-9);
    assert!((ws[1].used - 0.12).abs() < 1e-9);
    // On-demand is a currency ratio, not a percentage: 20 of 50 spent.
    assert!((ws[2].used - 0.4).abs() < 1e-9);
    // Every window shares the billing cycle end; Cursor has no per-window reset.
    for w in &ws {
        assert_eq!(w.resets_at, Some(CYCLE_END_MS));
    }
    assert!(note.is_empty());
}

#[test]
fn zero_included_usage_is_still_a_reading() {
    // Distinct from "no reading": a fresh billing cycle legitimately sits at 0 %, and the
    // ring must render empty rather than the cell disappearing.
    let v = json!({
        "billingCycleEnd": CYCLE_END_ISO,
        "individualUsage": { "plan": { "totalPercentUsed": 0.0 } }
    });
    let (ws, note) = parse_summary(&v);
    assert_eq!(ids(&ws), ["included"]);
    assert_eq!(ws[0].used, 0.0);
    assert!(note.is_empty());
}

#[test]
fn zero_api_usage_is_omitted() {
    // Asymmetric with included usage on purpose: most accounts never touch the API, and a
    // permanent 0 % ring is noise rather than information.
    let v = json!({
        "billingCycleEnd": CYCLE_END_ISO,
        "individualUsage": { "plan": { "totalPercentUsed": 10.0, "apiPercentUsed": 0.0 } }
    });
    assert_eq!(ids(&parse_summary(&v).0), ["included"]);
}

#[test]
fn on_demand_is_omitted_unless_it_is_enabled_and_bounded() {
    let base = |on_demand: serde_json::Value| {
        json!({
            "billingCycleEnd": CYCLE_END_ISO,
            "individualUsage": {
                "plan": { "totalPercentUsed": 10.0 },
                "onDemand": on_demand
            }
        })
    };
    // Disabled.
    assert_eq!(
        ids(&parse_summary(&base(json!({ "enabled": false, "limit": 50.0, "used": 20.0 }))).0),
        ["included"]
    );
    // Enabled with no ceiling: dividing by zero would produce NaN or infinity.
    assert_eq!(
        ids(&parse_summary(&base(json!({ "enabled": true, "limit": 0.0, "used": 20.0 }))).0),
        ["included"]
    );
    // Enabled and bounded but nothing spent yet is reported, unlike API usage: the user
    // opted in to on-demand spend, so its headroom is worth showing at zero.
    let (ws, _) = parse_summary(&base(json!({ "enabled": true, "limit": 50.0, "used": 0.0 })));
    assert_eq!(ids(&ws), ["included", "on_demand"]);
    assert_eq!(ws[1].used, 0.0);
    // Overspend clamps rather than drawing past the end of the ring.
    let (ws, _) = parse_summary(&base(json!({ "enabled": true, "limit": 50.0, "used": 80.0 })));
    assert_eq!(ws[1].used, 1.0);
}

#[test]
fn an_unlimited_plan_explains_itself_instead_of_showing_an_empty_ring() {
    let v = json!({ "membershipType": "enterprise", "isUnlimited": true });
    let (ws, note) = parse_summary(&v);
    assert!(ws.is_empty());
    assert_eq!(note, "Unlimited on the enterprise plan — nothing to meter");
}

#[test]
fn a_plan_with_nothing_to_meter_says_so() {
    let v = json!({ "membershipType": "free" });
    let (ws, note) = parse_summary(&v);
    assert!(ws.is_empty());
    assert_eq!(note, "The free plan has nothing for Cursor to meter yet");
}

#[test]
fn an_unknown_membership_type_still_produces_readable_copy() {
    // The plan name is optional, and the sentence has to survive its absence. Substituting
    // a placeholder into "the {} plan" used to print "The this plan has nothing...".
    assert_eq!(
        parse_summary(&json!({})).1,
        "This plan has nothing for Cursor to meter yet"
    );
    assert_eq!(
        parse_summary(&json!({ "membershipType": "" })).1,
        "This plan has nothing for Cursor to meter yet"
    );
    assert_eq!(
        parse_summary(&json!({ "isUnlimited": true })).1,
        "Unlimited on this plan — nothing to meter"
    );
}

#[test]
fn parse_summary_survives_shapes_it_has_never_seen() {
    for v in [
        json!(null),
        json!([]),
        json!({ "individualUsage": "not an object" }),
        json!({ "individualUsage": { "plan": "not an object" } }),
        json!({ "individualUsage": { "onDemand": 7 } }),
        json!({ "billingCycleEnd": 12345 }),
    ] {
        let (ws, note) = parse_summary(&v);
        assert!(ws.is_empty(), "unexpected windows for {v}");
        assert!(!note.is_empty(), "a reading with no windows must explain itself: {v}");
    }
}
