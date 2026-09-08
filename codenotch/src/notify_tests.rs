use super::*;

fn r<'a>(key: &'a str, used: f64) -> Reading<'a> {
    Reading { key, used }
}

/// The sequence that has to produce exactly one notification, which is the whole point of
/// keeping arming state at all.
#[test]
fn a_crossing_fires_once_and_not_again_until_it_drops_back() {
    let mut st = HashMap::new();
    // First reading primes. Starting the app at 90 % must be a number, not an alert about a
    // crossing that happened before the app was running.
    assert!(crossings(&mut st, &[r("claude:session", 0.91)], 0.80).is_empty());
    // Still above: already said.
    assert!(crossings(&mut st, &[r("claude:session", 0.93)], 0.80).is_empty());
    assert!(crossings(&mut st, &[r("claude:session", 0.99)], 0.80).is_empty());
    // Drops back: re-armed, silently.
    assert!(crossings(&mut st, &[r("claude:session", 0.10)], 0.80).is_empty());
    // And now it fires, once.
    assert_eq!(crossings(&mut st, &[r("claude:session", 0.82)], 0.80), vec!["claude:session"]);
    assert!(crossings(&mut st, &[r("claude:session", 0.84)], 0.80).is_empty());
}

/// A first reading that is *below* arms without firing, and the crossing after it fires. This
/// is the ordinary path and it must not be swallowed by the priming rule.
#[test]
fn a_first_reading_below_the_threshold_arms_rather_than_swallowing_the_next_crossing() {
    let mut st = HashMap::new();
    assert!(crossings(&mut st, &[r("claude:session", 0.20)], 0.80).is_empty());
    assert_eq!(crossings(&mut st, &[r("claude:session", 0.80)], 0.80), vec!["claude:session"]);
}

/// The threshold is inclusive: exactly at it is at it, not below it.
#[test]
fn the_threshold_itself_counts_as_crossed() {
    let mut st = HashMap::new();
    crossings(&mut st, &[r("w", 0.0)], 0.80);
    assert_eq!(crossings(&mut st, &[r("w", 0.80)], 0.80), vec!["w"]);
}

/// Windows are tracked separately. One provider's weekly window filling up must not silence
/// another provider's five-hour window doing the same, and the mod's own bug list has the
/// shared-state version of this mistake in it.
#[test]
fn each_window_is_armed_on_its_own() {
    let mut st = HashMap::new();
    crossings(&mut st, &[r("claude:session", 0.1), r("claude:weekly", 0.1), r("gemini:session", 0.1)], 0.80);
    let fired = crossings(
        &mut st,
        &[r("claude:session", 0.9), r("claude:weekly", 0.2), r("gemini:session", 0.95)],
        0.80,
    );
    assert_eq!(fired.len(), 2);
    assert!(fired.contains(&"claude:session".to_string()));
    assert!(fired.contains(&"gemini:session".to_string()));
    // The one that stayed low is still armed.
    assert_eq!(st["claude:weekly"], Arm::Below);
}

/// A window that disappears from a reading keeps its state rather than being re-primed. It
/// comes back when the window rolls over, and re-priming would silence the first crossing of
/// every new window.
#[test]
fn a_window_missing_from_one_reading_keeps_its_arming() {
    let mut st = HashMap::new();
    crossings(&mut st, &[r("a", 0.9)], 0.80); // primes as Fired
    crossings(&mut st, &[], 0.80); // absent
    assert_eq!(st["a"], Arm::Fired);
    assert!(crossings(&mut st, &[r("a", 0.95)], 0.80).is_empty());
}

/// The shell's buffers are sized in UTF-16 units, so truncation happens there. A cut that split
/// a surrogate pair would hand the shell a malformed string.
#[cfg(windows)]
#[test]
fn a_title_too_long_is_truncated_without_splitting_a_surrogate_pair() {
    let mut buf = [0u16; 8];
    write_utf16(&mut buf, "abcdefghijklmno");
    assert_eq!(String::from_utf16_lossy(&buf[..7]), "abcdefg");
    assert_eq!(buf[7], 0, "the buffer must stay terminated");

    // Four astral characters: two UTF-16 units each. A seven-unit budget can hold three whole
    // ones, and must not keep the high half of the fourth.
    let mut buf = [0u16; 8];
    write_utf16(&mut buf, "\u{1F600}\u{1F600}\u{1F600}\u{1F600}");
    let text = String::from_utf16(&buf[..6]).expect("must remain valid UTF-16");
    assert_eq!(text.chars().count(), 3);
    assert_eq!(buf[6], 0);

    // Shorter than the buffer: copied whole and terminated.
    let mut buf = [0u16; 8];
    write_utf16(&mut buf, "hi");
    assert_eq!(String::from_utf16_lossy(&buf[..2]), "hi");
    assert_eq!(buf[2], 0);
}
