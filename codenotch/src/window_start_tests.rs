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

// ---------------------------------------------------------------- the schedule

const HOUR: u64 = 3600;

#[test]
fn nothing_happens_while_the_toggle_is_off_or_nobody_is_signed_in() {
    assert_eq!(next_action(false, true, Some(5), None, NOW), Next::Wait(30));
    assert_eq!(next_action(true, false, Some(5), None, NOW), Next::Wait(30));
}

#[test]
fn a_fresh_reading_with_no_window_is_the_case_this_exists_for() {
    assert_eq!(next_action(true, true, Some(5), None, NOW), Next::Send);
}

/// An old reading is not wrong about *when* a reset happens - the time is absolute - but it can
/// be wrong about whether a window exists at all, and acting on that spends a message.
#[test]
fn a_stale_reading_is_never_acted_on() {
    assert_eq!(next_action(true, true, Some(601), None, NOW), Next::Wait(30));
    assert_eq!(next_action(true, true, None, None, NOW), Next::Wait(30));
    // Exactly at the limit is still fresh; the boundary belongs to the usable side.
    assert_eq!(next_action(true, true, Some(600), None, NOW), Next::Send);
}

#[test]
fn it_waits_out_a_running_window_and_fires_just_after_the_reset() {
    // Four hours to go: idle ticking, nothing clever.
    assert_eq!(next_action(true, true, Some(5), Some(NOW + 4 * HOUR), NOW), Next::Wait(30));
    // Inside the last half minute it sleeps to the moment itself, so the message lands within
    // seconds of the reset rather than up to a tick after it.
    assert_eq!(next_action(true, true, Some(5), Some(NOW + 5), NOW), Next::Wait(15));
    assert_eq!(next_action(true, true, Some(5), Some(NOW - 5), NOW), Next::Wait(5));
    // The reset itself is not the moment: arriving early would reopen the old window and waste
    // the message, because this machine's clock is not the one that decides the boundary.
    assert_eq!(next_action(true, true, Some(5), Some(NOW), NOW), Next::Wait(10));
    assert_eq!(next_action(true, true, Some(5), Some(NOW - 9), NOW), Next::Wait(1));
    assert_eq!(next_action(true, true, Some(5), Some(NOW - 10), NOW), Next::Send);
    assert_eq!(next_action(true, true, Some(5), Some(NOW - HOUR), NOW), Next::Send);
}
