use super::*;

const NOW: u64 = 1_000_000;

#[test]
fn a_borrowed_credential_is_never_used_to_send_anything() {
    let reason = why_not(false, None, 0, NOW).expect("must refuse");
    assert!(reason.contains("Sign in"));
    assert!(
        reason.contains("borrowed"),
        "the refusal has to say why, or it reads as a bug: {reason}"
    );
}

#[test]
fn a_running_window_is_not_restarted() {
    // Two hours left: starting one now would spend a message and change nothing.
    let reason = why_not(true, Some(NOW + 2 * 3600 + 15 * 60), 0, NOW).expect("must refuse");
    assert!(reason.contains("2h 15m"), "the time left has to be in it: {reason}");

    // A window that has already reset is not a running window.
    assert!(why_not(true, Some(NOW - 1), 0, NOW).is_none());
    // No window at all is the case this feature exists for.
    assert!(why_not(true, None, 0, NOW).is_none());
}

#[test]
fn a_second_click_inside_five_minutes_does_nothing() {
    assert!(why_not(true, None, NOW - 10, NOW).is_some());
    assert!(why_not(true, None, NOW - 299, NOW).is_some());
    assert!(why_not(true, None, NOW - 300, NOW).is_none());
    // Never sent is not "sent at the epoch": a fresh install must not be blocked.
    assert!(why_not(true, None, 0, NOW).is_none());
}

/// The reasons are ordered so the most fundamental one wins. Being told "try again in 4m" when
/// the real problem is that you are not signed in would send someone in the wrong direction.
#[test]
fn the_most_fundamental_reason_is_the_one_reported() {
    let reason = why_not(false, Some(NOW + 3600), NOW - 5, NOW).unwrap();
    assert!(reason.contains("Sign in"));
}

/// The cap is the mechanism, not the wording. A model that ignores the system line still cannot
/// produce a long reply, and that is the property worth pinning.
#[test]
fn the_request_is_the_smallest_thing_that_counts_as_a_message() {
    let b = body();
    assert_eq!(b["model"], MODEL);
    assert_eq!(b["max_tokens"], MAX_TOKENS);
    assert_eq!(b["messages"].as_array().unwrap().len(), 1);
    assert_eq!(b["messages"][0]["role"], "user");
    assert_eq!(b["messages"][0]["content"], "hi");
    const { assert!(MAX_TOKENS <= 8, "a cap that allows a paragraph is not a cap") };
    // The instruction is carried in every request because there is no conversation to keep it
    // in - that is the correction this module is built around, so it is worth a test.
    assert!(b["system"].as_str().unwrap().contains("Noted"));
}
