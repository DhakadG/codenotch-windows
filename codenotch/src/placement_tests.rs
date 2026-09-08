use super::*;

/// 3840x2160 at the origin, and a second one to its left with negative coordinates - the
/// arrangement that catches sign errors, because "left of primary" means x is negative rather
/// than small.
const MAIN: Rect = Rect { x: 0, y: 0, w: 3840, h: 2160 };
const LEFT_OF_MAIN: Rect = Rect { x: -1920, y: 0, w: 1920, h: 1080 };
const ABOVE_MAIN: Rect = Rect { x: 0, y: -1080, w: 1920, h: 1080 };

const SIDE: (i32, i32) = (425, 575); // 340x460 at 125 %
const FLAT: (i32, i32) = (575, 425);

#[test]
fn each_edge_puts_the_window_against_that_edge() {
    assert_eq!(window_origin(MAIN, Edge::Right, 0.5, SIDE).0, 3840 - 425);
    assert_eq!(window_origin(MAIN, Edge::Left, 0.5, SIDE).0, 0);
    assert_eq!(window_origin(MAIN, Edge::Top, 0.5, FLAT).1, 0);
    assert_eq!(window_origin(MAIN, Edge::Bottom, 0.5, FLAT).1, 2160 - 425);
}

#[test]
fn the_position_along_the_edge_follows_the_axis_the_edge_runs_on() {
    // Vertical edges move the window up and down.
    let (_, top) = window_origin(MAIN, Edge::Right, 0.0, SIDE);
    let (_, mid) = window_origin(MAIN, Edge::Right, 0.5, SIDE);
    let (_, bot) = window_origin(MAIN, Edge::Right, 1.0, SIDE);
    assert_eq!(top, 0);
    assert_eq!(mid, 2160 / 2 - 575 / 2);
    assert_eq!(bot, 2160 - 575);
    // Horizontal edges move it left and right, and the *other* coordinate stays pinned.
    let (l, y0) = window_origin(MAIN, Edge::Top, 0.0, FLAT);
    let (r, y1) = window_origin(MAIN, Edge::Top, 1.0, FLAT);
    assert_eq!((l, y0), (0, 0));
    assert_eq!((r, y1), (3840 - 575, 0));
}

/// A monitor arranged left of or above the primary has negative coordinates. Treating those as
/// zero is the classic multi-monitor bug and it puts the window on the wrong screen entirely.
#[test]
fn negative_monitor_coordinates_are_respected() {
    assert_eq!(window_origin(LEFT_OF_MAIN, Edge::Left, 0.5, SIDE).0, -1920);
    assert_eq!(window_origin(LEFT_OF_MAIN, Edge::Right, 0.5, SIDE).0, -1920 + 1920 - 425);
    assert_eq!(window_origin(ABOVE_MAIN, Edge::Top, 0.5, FLAT).1, -1080);
    assert_eq!(window_origin(ABOVE_MAIN, Edge::Bottom, 0.5, FLAT).1, -1080 + 1080 - 425);
}

/// A window taller than its monitor is not hypothetical: 460 pt at 200 % is 920 physical
/// pixels, and a 768-tall secondary display cannot hold it. The clamp must pin it to the top
/// rather than wrap to a huge negative number.
#[test]
fn a_window_bigger_than_the_monitor_is_pinned_rather_than_wrapped() {
    let small = Rect { x: 100, y: 200, w: 400, h: 300 };
    let (x, y) = window_origin(small, Edge::Right, 1.0, (425, 575));
    assert_eq!(y, 200, "clamped to the monitor's top, not below its bottom");
    assert_eq!(x, 100 + 400 - 425, "the edge is still the edge, even overhanging");
    let (x2, _) = window_origin(small, Edge::Top, 1.0, (575, 425));
    assert_eq!(x2, 100);
}

/// A drag writes a fraction and the next redraw reads it. If these two disagree the pill creeps
/// on every restart, which is the kind of bug that takes a week to notice and an hour to trust.
#[test]
fn the_position_survives_a_round_trip_through_a_drag() {
    for edge in [Edge::Right, Edge::Left, Edge::Top, Edge::Bottom] {
        let win = if edge.is_vertical() { SIDE } else { FLAT };
        for along in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let origin = window_origin(MAIN, edge, along, win);
            let back = along_from_origin(MAIN, edge, origin, win);
            let regained = window_origin(MAIN, edge, back, win);
            assert_eq!(
                origin, regained,
                "{edge:?} at {along} moved on the round trip"
            );
        }
    }
}

#[test]
fn an_edge_decides_the_window_shape() {
    assert_eq!(Edge::Right.size(), SIDE_SIZE);
    assert_eq!(Edge::Left.size(), SIDE_SIZE);
    assert_eq!(Edge::Top.size(), FLAT_SIZE);
    assert_eq!(Edge::Bottom.size(), FLAT_SIZE);
    // Unknown text is the edge this app has always used, not a panic and not nothing: a typo in
    // a config file must not leave the pill somewhere nobody can find it.
    assert_eq!(Edge::parse("sideways"), Edge::Right);
    assert_eq!(Edge::parse(""), Edge::Right);
    for e in [Edge::Right, Edge::Left, Edge::Top, Edge::Bottom] {
        assert_eq!(Edge::parse(e.as_str()), e, "{e:?} must survive a round trip");
    }
}

// ---------------------------------------------------------------- choosing a monitor

fn screens() -> Vec<(String, Rect, bool)> {
    vec![
        ("\\\\.\\DISPLAY1".into(), MAIN, true),
        ("\\\\.\\DISPLAY2".into(), LEFT_OF_MAIN, false),
    ]
}

#[test]
fn the_cursor_target_follows_the_pointer_between_screens() {
    let s = screens();
    assert_eq!(choose_monitor(&Target::Cursor, &s, Some((100, 100))).unwrap().1, MAIN);
    assert_eq!(choose_monitor(&Target::Cursor, &s, Some((-500, 100))).unwrap().1, LEFT_OF_MAIN);
    // A pointer nowhere - between monitors, or unreadable - is not a reason to move anything.
    assert_eq!(choose_monitor(&Target::Cursor, &s, Some((9999, 9999))).unwrap().1, MAIN);
    assert_eq!(choose_monitor(&Target::Cursor, &s, None).unwrap().1, MAIN);
}

/// The case that decides whether this feature is safe to ship: a remembered display that is no
/// longer attached. Falling back to the primary keeps the pill somewhere the user can see it and
/// change the setting; honouring the stale coordinates puts it on a screen that does not exist.
#[test]
fn a_named_monitor_that_is_gone_falls_back_to_the_primary() {
    let s = screens();
    assert_eq!(
        choose_monitor(&Target::Named("\\\\.\\DISPLAY2".into()), &s, None).unwrap().1,
        LEFT_OF_MAIN
    );
    assert_eq!(
        choose_monitor(&Target::Named("\\\\.\\DISPLAY7".into()), &s, None).unwrap().1,
        MAIN,
        "an unplugged display must not strand the pill"
    );
}

#[test]
fn no_monitors_at_all_is_not_a_panic() {
    assert!(choose_monitor(&Target::Primary, &[], None).is_none());
    assert!(choose_monitor(&Target::Cursor, &[], Some((0, 0))).is_none());
}

/// Windows can report no primary flag at all in odd configurations. Something has to be chosen.
#[test]
fn a_set_with_no_primary_still_yields_a_monitor() {
    let s = vec![("\\\\.\\DISPLAY9".to_string(), LEFT_OF_MAIN, false)];
    assert_eq!(choose_monitor(&Target::Primary, &s, None).unwrap().1, LEFT_OF_MAIN);
}

#[test]
fn a_target_round_trips_through_its_text_form() {
    assert_eq!(Target::parse("primary"), Target::Primary);
    assert_eq!(Target::parse(""), Target::Primary);
    assert_eq!(Target::parse("cursor"), Target::Cursor);
    assert_eq!(Target::parse("\\\\.\\DISPLAY2"), Target::Named("\\\\.\\DISPLAY2".into()));
}

// ---------------------------------------------------------------- getting out of the way

#[test]
fn full_screen_on_another_monitor_does_not_hide_the_pill() {
    let game = (LEFT_OF_MAIN, LEFT_OF_MAIN);
    assert!(!should_hide(true, Some(game), MAIN), "the pill is on the other screen");
    assert!(should_hide(true, Some(game), LEFT_OF_MAIN), "and hides when it is on that one");
}

/// Maximised is not full screen. A maximised window fills the work area and stops short of the
/// taskbar; hiding for that would make the pill useless on any machine where windows are
/// normally maximised, which is most of them.
#[test]
fn a_maximised_window_is_not_full_screen() {
    let maximised = Rect { x: 0, y: 0, w: 3840, h: 2160 - 48 };
    assert!(!should_hide(true, Some((maximised, MAIN)), MAIN));
}

#[test]
fn a_player_overhanging_its_monitor_still_counts() {
    // Some players sit a pixel outside on one side; a strict equality test would call that
    // windowed and leave the pill on top of a film.
    let over = Rect { x: -1, y: -1, w: 3842, h: 2162 };
    assert!(should_hide(true, Some((over, MAIN)), MAIN));
}

#[test]
fn nothing_hides_while_the_setting_is_off_or_nothing_is_in_front() {
    assert!(!should_hide(false, Some((MAIN, MAIN)), MAIN));
    assert!(!should_hide(true, None, MAIN));
}
