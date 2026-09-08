use super::*;

/// The three shapes people actually paste. The callback page shows `code#state`, but plenty of
/// people copy the whole address bar instead, and some copy only the code.
#[test]
fn pasted_code_is_read_in_every_form_people_paste_it() {
    assert_eq!(split_pasted("abc#xyz"), ("abc".into(), "xyz".into()));
    assert_eq!(split_pasted("  abc#xyz  "), ("abc".into(), "xyz".into()));
    // Quotes come from copying out of a terminal or a JSON blob.
    assert_eq!(split_pasted("\"abc#xyz\""), ("abc".into(), "xyz".into()));
    // No state: accepted, because the callback page does not always show one.
    assert_eq!(split_pasted("abc"), ("abc".into(), String::new()));
    // Full callback URL, state in the query.
    assert_eq!(
        split_pasted("https://console.anthropic.com/oauth/code/callback?code=abc&state=xyz"),
        ("abc".into(), "xyz".into())
    );
    // Full URL whose code still carries the fragment form, which is what the page produces
    // when the fragment survives the copy.
    assert_eq!(
        split_pasted("https://console.anthropic.com/oauth/code/callback?code=abc%23xyz")
            .0
            .as_str(),
        "abc%23xyz"
    );
    assert_eq!(
        split_pasted("https://console.anthropic.com/oauth/code/callback?code=abc#xyz"),
        ("abc".into(), "xyz".into())
    );
}

/// base64url, no padding: the alphabet PKCE requires. Getting `+/` instead of `-_` here would
/// produce a challenge the server rejects, and the failure would look like a bad code.
#[test]
fn b64url_uses_the_url_safe_alphabet_without_padding() {
    assert_eq!(b64url(b""), "");
    assert_eq!(b64url(b"f"), "Zg");
    assert_eq!(b64url(b"fo"), "Zm8");
    assert_eq!(b64url(b"foo"), "Zm9v");
    assert_eq!(b64url(b"foob"), "Zm9vYg");
    assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
    // The two bytes that separate base64url from base64: 0xFB 0xFF encodes to +/ in standard.
    let encoded = b64url(&[0xfb, 0xef, 0xbe]);
    assert!(!encoded.contains('+') && !encoded.contains('/'), "got {encoded}");
    assert!(!encoded.contains('='), "padding must be absent, got {encoded}");
}

#[test]
fn urlencode_leaves_the_unreserved_set_alone_and_escapes_the_rest() {
    assert_eq!(urlencode("abcXYZ019-_.~"), "abcXYZ019-_.~");
    assert_eq!(urlencode("a b"), "a%20b");
    assert_eq!(
        urlencode("https://console.anthropic.com/oauth/code/callback"),
        "https%3A%2F%2Fconsole.anthropic.com%2Foauth%2Fcode%2Fcallback"
    );
    // The scope is space-separated and goes in a query parameter, so its spaces must escape.
    assert_eq!(urlencode("org:create_api_key user:profile"), "org%3Acreate_api_key%20user%3Aprofile");
}

#[test]
fn token_response_carries_its_expiry_forward_from_now() {
    let t = parse_token_response(
        r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#,
        1_000,
    )
    .expect("valid response");
    assert_eq!(t.access_token, "at");
    assert_eq!(t.refresh_token, "rt");
    assert_eq!(t.expires_at, 4_600);
    assert!(!t.needs_refresh(1_000));
    // Inside the skew window, so it refreshes rather than sending a token about to die.
    assert!(t.needs_refresh(4_400));
    assert!(t.needs_refresh(4_600));
}

/// No `expires_in` means unknown, and unknown must read as "refresh first". Treating it as
/// "never expires" would send a dead token forever and look like a broken sign-in.
#[test]
fn a_response_without_an_expiry_is_treated_as_already_due() {
    let t = parse_token_response(r#"{"access_token":"at","refresh_token":"rt"}"#, 1_000).unwrap();
    assert_eq!(t.expires_at, 0);
    assert!(t.needs_refresh(0));
    assert!(t.needs_refresh(u64::MAX / 2));
}

#[test]
fn a_response_without_an_access_token_is_an_error_not_an_empty_session() {
    assert!(parse_token_response(r#"{"error":"invalid_grant"}"#, 0).is_err());
    assert!(parse_token_response(r#"{"access_token":""}"#, 0).is_err());
    assert!(parse_token_response("not json at all", 0).is_err());
}

/// The digest has to be of the verifier *string*, not of the random bytes behind it - PKCE
/// hashes the ASCII the client sends. Hashing the wrong thing yields a challenge the server
/// rejects with a message about the code, which sends debugging in the wrong direction.
#[cfg(windows)]
#[test]
fn sha256_matches_the_published_vector() {
    // SHA-256("abc"), the vector from FIPS 180-4.
    let d = sha256(b"abc");
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[cfg(windows)]
#[test]
fn random_bytes_are_the_requested_length_and_not_all_one_value() {
    let b = random_bytes(32);
    assert_eq!(b.len(), 32);
    assert!(b.iter().any(|x| *x != b[0]), "32 identical bytes is not randomness");
}
