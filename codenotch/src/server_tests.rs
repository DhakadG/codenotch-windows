use super::*;

/// The sign-in form posts one field. A code containing `+` or `%` must survive the round trip:
/// `+` means a space in a form body, and the codes Anthropic issues are base64url, which can
/// legitimately contain characters the browser escapes.
#[test]
fn form_field_decodes_what_a_browser_sends() {
    assert_eq!(form_field("code=abc", "code"), "abc");
    assert_eq!(form_field("code=abc%23xyz", "code"), "abc#xyz");
    assert_eq!(form_field("other=1&code=abc&z=2", "code"), "abc");
    assert_eq!(form_field("code=a+b", "code"), "a b");
    assert_eq!(form_field("", "code"), "");
    assert_eq!(form_field("nocode=1", "code"), "");
    // A field present but empty is empty, not missing - both end up rejected upstream, but
    // silently turning one into the other hides which happened.
    assert_eq!(form_field("code=", "code"), "");
}

/// A stray `%` must not eat the rest of the string. The pasted code should reach the token
/// endpoint intact and be rejected there with a clear reason, rather than be mangled here and
/// blamed on the user.
#[test]
fn percent_decode_leaves_a_malformed_escape_alone() {
    assert_eq!(percent_decode("100%"), "100%");
    assert_eq!(percent_decode("a%zz b"), "a%zz b");
    assert_eq!(percent_decode("%41%42"), "AB");
    assert_eq!(percent_decode("%2"), "%2");
}

/// The page has to say which of the three things happened, because "did that work?" is the
/// only question anyone has while looking at it.
#[test]
fn the_signin_page_reports_each_outcome_distinctly() {
    let fresh = signin_page(None);
    assert!(fresh.contains("<form method=post"));
    assert!(!fresh.contains("class=ok") && !fresh.contains("class=bad"));

    let good = signin_page(Some(Ok(())));
    assert!(good.contains("class=ok"));
    assert!(good.contains("Signed in"));

    let bad = signin_page(Some(Err("Anthropic refused the sign-in (400)".into())));
    assert!(bad.contains("class=bad"));
    assert!(bad.contains("Anthropic refused"));
}

/// An error message is server-supplied text placed into HTML. It is not attacker-controlled in
/// any meaningful sense on a loopback page, but escaping it costs one function and removes the
/// question entirely.
#[test]
fn an_error_is_escaped_into_the_page() {
    let page = signin_page(Some(Err("<script>alert(1)</script>".into())));
    assert!(!page.contains("<script>"));
    assert!(page.contains("&lt;script&gt;"));
}
