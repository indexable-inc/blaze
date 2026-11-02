use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use blaze_ids::rng::SplitMix64;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};

use super::*;

/// A TEST KEY, never an App's: the 2048-bit two-prime key RustCrypto's `pkcs1` crate publishes
/// for its tests (`tests/examples/rsa2048-priv.pem`).
const TEST_KEY_PKCS1: &str = "\
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAtsQsUV8QpqrygsY+2+JCQ6Fw8/omM71IM2N/R8pPbzbgOl0p
78MZGsgPOQ2HSznjD0FPzsH8oO2B5Uftws04LHb2HJAYlz25+lN5cqfHAfa3fgmC
38FfwBkn7l582UtPWZ/wcBOnyCgb3yLcvJrXyrt8QxHJgvWO23ITrUVYszImbXQ6
7YGS0YhMrbixRzmo2tpm3JcIBtnHrEUMsT0NfFdfsZhTT8YbxBvA8FdODgEwx7u/
vf3J9qbi4+Kv8cvqyJuleIRSjVXPsIMnoejIn04APPKIjpMyQdnWlby7rNyQtE4+
CV+jcFjqJbE/Xilcvqxt6DirjFCvYeKYl1uHLwIDAQABAoIBAH7Mg2LA7bB0EWQh
XiL3SrnZG6BpAHAM9jaQ5RFNjua9z7suP5YUaSpnegg/FopeUuWWjmQHudl8bg5A
ZPgtoLdYoU8XubfUH19I4o1lUXBPVuaeeqn6Yw/HZCjAbSXkVdz8VbesK092ZD/e
0/4V/3irsn5lrMSq0L322yfvYKaRDFxKCF7UMnWrGcHZl6Msbv/OffLRk19uYB7t
4WGhK1zCfKIfgdLJnD0eoI6Q4wU6sJvvpyTe8NDDo8HpdAwNn3YSahSewKp9gHgg
VIQlTZUdsHxM+R+2RUwJZYj9WSTbq+s1nKICUmjQBPnWbrPW963BE5utQPFt3mOe
EWRzdsECgYEA3MBhJC1Okq+u5yrFE8plufdwNvm9fg5uYUYafvdlQiXsFTx+XDGm
FXpuWhP/bheOh1jByzPZ1rvjF57xiZjkIuzcvtePTs/b5fT82K7CydDchkc8qb0W
2dI40h+13e++sUPKYdC9aqjZHzOgl3kOlkDbyRCF3F8mNDujE49rLWcCgYEA0/MU
dX5A6VSDb5K+JCNq8vDaBKNGU8GAr2fpYAhtk/3mXLI+/Z0JN0di9ZgeNhhJr2jN
11OU/2pOButpsgnkIo2y36cOQPf5dQpSgXZke3iNDld3osuLIuPNJn/3C087AtOq
+w4YxZClZLAxiLCqX8SBVrB2IiFCQ70SJ++n8vkCgYEAzmi3rBsNEA1jblVIh1PF
wJhD/bOQ4nBd92iUV8m9jZdl4wl4YX4u/IBI9MMkIG24YIe2VOl7s9Rk5+4/jNg/
4QQ2998Y6aljxOZJEdZ+3jQELy4m49OhrTRq2ta5t/Z3CMsJTmLe6f9NXWZpr5iK
8iVdHOjtMXxqfYaR2jVNEtsCgYAl9uWUQiAoa037v0I1wO5YQ9IZgJGJUSDWynsg
C4JtPs5zji4ASY+sCipsqWnH8MPKGrC8QClxMr51ONe+30yw78a5jvfbpU9Wqpmq
vOU0xJwnlH1GeMUcY8eMfOFocjG0yOtYeubvBIDLr0/AFzz9WHp+Z69RX7m53nUR
GDlyKQKBgDGZVAbUBiB8rerqNbONBAxfipoa4IJ+ntBrFT2DtoIZNbSzaoK+nVbH
kbWMJycaV5PVOh1lfAiZeWCxQz5RcZh/RS8USnxyMG1j4dP/wLcbdasI8uRaSC6Y
hFHL5HjhLrIo0HRWySS2b2ztBI2FP1M+MaaGFPHDzm2OyZg85yr3
-----END RSA PRIVATE KEY-----
";

fn der_of(pem: &str) -> Vec<u8> {
    let base64: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
    STANDARD.decode(base64).unwrap()
}

fn der_length(len: usize) -> Vec<u8> {
    match len {
        0..128 => vec![len as u8],
        128..256 => vec![0x81, len as u8],
        _ => vec![0x82, (len >> 8) as u8, len as u8],
    }
}

fn pem(label: &str, der: &[u8]) -> String {
    let base64 = STANDARD.encode(der);
    let lines: Vec<&str> = base64.as_bytes().chunks(64).map(|it| std::str::from_utf8(it).unwrap()).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

/// The test key in a PKCS#8 wrapper: version 0, rsaEncryption with NULL parameters, the
/// PKCS#1 key as an octet string.
fn test_key_pkcs8() -> String {
    let pkcs1 = der_of(TEST_KEY_PKCS1);
    let mut inner = vec![0x02, 0x01, 0x00];
    inner.extend([0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00]);
    inner.push(0x04);
    inner.extend(der_length(pkcs1.len()));
    inner.extend(&pkcs1);
    let mut der = vec![0x30];
    der.extend(der_length(inner.len()));
    der.extend(inner);
    pem("PRIVATE KEY", &der)
}

fn test_key() -> Key {
    Key::from_pem(TEST_KEY_PKCS1).unwrap()
}

#[test]
fn a_pkcs1_key_parses() {
    let key = test_key();
    assert_eq!(key.0.public().modulus_len(), 256);
}

#[test]
fn a_pkcs8_key_parses_to_the_same_key() {
    let key = Key::from_pem(&test_key_pkcs8()).unwrap();
    assert_eq!(key.0.public().as_ref(), test_key().0.public().as_ref());
}

#[test]
fn a_key_with_blank_lines_and_crlf_around_it_parses() {
    let pem = format!("\n\r\n{}\r\n", TEST_KEY_PKCS1.replace('\n', "\r\n"));
    assert!(Key::from_pem(&pem).is_ok());
}

#[test]
fn other_pems_are_refused_by_name() {
    let error = Key::from_pem(&pem("EC PRIVATE KEY", &der_of(TEST_KEY_PKCS1))).unwrap_err();
    assert!(error.what.contains("EC PRIVATE KEY"), "{}", error.what);
    let encrypted = TEST_KEY_PKCS1.replacen(
        "-----\n",
        "-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00000000000000000000000000000000\n\n",
        1,
    );
    assert!(Key::from_pem(&encrypted).unwrap_err().what.contains("encrypted"));
    let unended = TEST_KEY_PKCS1.replace("-----END RSA PRIVATE KEY-----", "");
    assert!(Key::from_pem(&unended).unwrap_err().what.contains("END"));
    assert!(Key::from_pem("not a key").is_err());
    let swapped = TEST_KEY_PKCS1.replace("RSA PRIVATE KEY", "PRIVATE KEY");
    assert!(Key::from_pem(&swapped).is_err(), "a PKCS#1 body under a PKCS#8 label is refused");
}

#[test]
fn a_refused_key_names_no_byte_of_it() {
    let mut der = der_of(TEST_KEY_PKCS1);
    der.pop();
    let text = pem("RSA PRIVATE KEY", &der);
    let error = Key::from_pem(&text).unwrap_err();
    let body: String = text.lines().filter(|line| !line.starts_with("-----")).collect();
    for line in body.as_bytes().chunks(16) {
        assert!(!error.what.contains(std::str::from_utf8(line).unwrap()), "{}", error.what);
    }
}

#[test]
fn a_jwt_decodes_as_written_and_its_signature_verifies() {
    let key = test_key();
    let now = 1_800_000_000;
    let jwt = key.jwt(123_456, now).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    assert!(!jwt.contains('='), "base64url without padding");

    let header = URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
    assert_eq!(header, br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims, json!({ "iat": now - 60, "exp": now + 540, "iss": 123_456 }));

    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.0.public().as_ref());
    let signed = format!("{}.{}", parts[0], parts[1]);
    assert!(public.verify(signed.as_bytes(), &signature).is_ok());

    let forged = URL_SAFE_NO_PAD.encode(br#"{"iat":0,"exp":9999999999,"iss":123456}"#);
    let forged = format!("{}.{forged}", parts[0]);
    assert!(public.verify(forged.as_bytes(), &signature).is_err());
}

#[test]
fn debug_and_display_hide_the_key_and_the_token() {
    let key = test_key();
    let app = App { id: 1, installation: 2, key, api: "api.github.com".to_owned() };
    let shown = format!("{app:?} {} {:?}", app.key, app.key);
    let body: String = TEST_KEY_PKCS1.lines().filter(|line| !line.starts_with("-----")).collect();
    for line in body.as_bytes().chunks(16) {
        assert!(!shown.contains(std::str::from_utf8(line).unwrap()), "{shown}");
    }
    assert!(!shown.contains("BEGIN"));

    let token = Token::new("ghs_16C7e42F292c6912E7710c838347Ae178B4a".to_owned());
    let shown = format!("{token:?} {:?}", Some(token.clone()));
    assert!(!shown.contains("ghs_"), "{shown}");
    assert!(!shown.contains("16C7e42F"), "{shown}");
    assert_eq!(token.expose(), "ghs_16C7e42F292c6912E7710c838347Ae178B4a");
}

fn run(status: CheckStatus) -> CheckRun {
    CheckRun {
        name: "blaze/test".to_owned(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        status,
        summary: "3 passed, 0 failed".to_owned(),
    }
}

#[test]
fn the_check_run_json_of_each_status() {
    let cases = [
        (CheckStatus::Queued, "queued", None, "Queued"),
        (CheckStatus::InProgress, "in_progress", None, "Running"),
        (CheckStatus::Success, "completed", Some("success"), "Passed"),
        (CheckStatus::Failure, "completed", Some("failure"), "Failed"),
        (CheckStatus::Cancelled, "completed", Some("cancelled"), "Cancelled"),
    ];
    for (status, word, conclusion, title) in cases {
        let mut want = json!({
            "name": "blaze/test",
            "head_sha": "0123456789abcdef0123456789abcdef01234567",
            "status": word,
            "output": { "title": title, "summary": "3 passed, 0 failed" },
        });
        if let Some(conclusion) = conclusion {
            want["conclusion"] = json!(conclusion);
        }
        assert_eq!(body(&run(status), true), want, "{status:?}");

        want.as_object_mut().unwrap().remove("head_sha");
        assert_eq!(body(&run(status), false), want, "{status:?} as an update");
    }
}

/// [`check_run`]'s bytes, parsed.
fn body(run: &CheckRun, create: bool) -> Value {
    let mut out = vec![b'x'; 7];
    check_run(&mut out, run, create);
    serde_json::from_slice(&out).unwrap()
}

/// What [`summary`] sends: the text kept, then the note.
fn summarized(text: &str) -> String {
    let (kept, cut) = summary(text);
    format!("{kept}{cut}")
}

#[test]
fn a_short_summary_is_kept_whole() {
    let text = "x".repeat(MOST_SUMMARY);
    assert_eq!(summary(&text), (text.as_str(), ""));
}

#[test]
fn a_long_summary_is_cut_at_a_char_boundary_saying_so() {
    let text = "é".repeat(MOST_SUMMARY);
    let cut = summarized(&text);
    assert!(cut.len() <= MOST_SUMMARY);
    assert!(cut.chars().count() <= MOST_SUMMARY);
    assert!(cut.ends_with(CUT));
    let kept = cut.strip_suffix(CUT).unwrap();
    assert!(kept.chars().all(|it| it == 'é'));
    assert!(kept.len() >= MOST_SUMMARY - CUT.len() - 1);

    let ascii = "y".repeat(MOST_SUMMARY + 1);
    assert_eq!(summarized(&ascii).len(), MOST_SUMMARY);
}

#[test]
fn a_cut_summary_goes_into_the_check_run() {
    let mut long = run(CheckStatus::Failure);
    long.summary = "z".repeat(100_000);
    let body = body(&long, true);
    let summary = body["output"]["summary"].as_str().unwrap();
    assert!(summary.len() <= MOST_SUMMARY);
    assert!(summary.ends_with(CUT));
}

/// A summary of every hard case: each byte below 0x20, quotes and backslashes at every offset
/// in a word, DEL, multi-byte characters, and runs long enough to take the eight-byte path.
fn hard_texts() -> Vec<String> {
    let mut texts = vec![String::new(), "plain ascii only, longer than a word".to_owned()];
    texts.push((0u8..0x20).map(char::from).collect());
    for at in 0..9 {
        let mut text = "a".repeat(at);
        text.push('"');
        text.push_str(&"b".repeat(17 - at));
        text.push('\\');
        texts.push(text);
    }
    texts.push("\u{7f}é—😀\u{1}\u{1f}\u{2028}x".repeat(5));
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for _ in 0..200 {
        let len = rng.index(70);
        let text: String = (0..len)
            .map(|_| {
                let seed = rng.next_u64();
                match seed % 6 {
                    0 => char::from((seed >> 8) as u8 % 0x20),
                    1 => ['"', '\\', '/', '\u{7f}'][(seed >> 8) as usize % 4],
                    2 => char::from_u32(0x80 + (seed >> 8) as u32 % 0x700).unwrap_or('é'),
                    _ => char::from(b' ' + (seed >> 8) as u8 % 95),
                }
            })
            .collect();
        texts.push(text);
    }
    texts
}

#[test]
fn json_strings_are_escaped_byte_for_byte_as_serde_json_escapes_them() {
    for text in hard_texts() {
        let mut out = Vec::new();
        json_string(&mut out, &[&text]);
        assert_eq!(String::from_utf8(out).unwrap(), serde_json::to_string(&text).unwrap(), "{text:?}");
    }
    let mut out = Vec::new();
    json_string(&mut out, &["a\"", "", "\nb"]);
    assert_eq!(out, serde_json::to_string("a\"\nb").unwrap().into_bytes());
}

#[test]
fn the_word_test_finds_every_byte_that_needs_escaping() {
    for byte in 0..=255u8 {
        for at in 0..8 {
            let mut word = [b'a'; 8];
            word[at] = byte;
            let needs = byte < 0x20 || byte == b'"' || byte == b'\\';
            assert_eq!(needs_escape(u64::from_le_bytes(word)), needs, "{byte:#x} at {at}");
            let mut high = [0xc3; 8];
            high[at] = byte;
            assert_eq!(needs_escape(u64::from_le_bytes(high)), needs, "{byte:#x} at {at} among 0xc3");
        }
    }
}

#[test]
fn a_check_run_body_has_every_hard_summary_whole() {
    for text in hard_texts() {
        let mut check = run(CheckStatus::Success);
        check.summary = text.clone();
        check.name = text.clone();
        let body = body(&check, true);
        assert_eq!(body["output"]["summary"], Value::String(text.clone()));
        assert_eq!(body["name"], Value::String(text));
    }
}

#[test]
fn requests_are_written_as_the_api_crate_writes_them() {
    let path = PathOf { route: Route::CheckRun(42), installation: 7, repo: "o/r" };
    let mut out = vec![1, 2, 3];
    write_request(&mut out, "PATCH", "api.github.com", &path, ("token ", "t0k"), b"{\"a\":1}");
    let want = http::request(
        "PATCH",
        "api.github.com",
        "/repos/o/r/check-runs/42",
        &[
            ("Authorization", "token t0k"),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "blaze-ci"),
            ("Content-Type", "application/json"),
        ],
        b"{\"a\":1}",
    );
    assert_eq!(out, want);

    let path = PathOf { route: Route::Mint, installation: 7, repo: "o/r" };
    write_request(&mut out, "POST", "api.github.com", &path, ("Bearer ", "j.w.t"), b"");
    let want = http::request(
        "POST",
        "api.github.com",
        "/app/installations/7/access_tokens",
        &[
            ("Authorization", "Bearer j.w.t"),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "blaze-ci"),
        ],
        b"",
    );
    assert_eq!(out, want);

    let path = PathOf { route: Route::CheckRuns, installation: 7, repo: "o/r" };
    assert_eq!(path.to_string(), "/repos/o/r/check-runs");
}

#[test]
fn expires_at_parses_in_utc() {
    assert_eq!(unix_seconds("2016-07-11T22:14:10Z"), Ok(1_468_275_250));
    assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Ok(0));
    assert_eq!(unix_seconds("2000-03-01T00:00:00Z"), Ok(951_868_800));
    assert_eq!(unix_seconds("2024-02-29T12:00:00Z"), Ok(1_709_208_000));
    assert_eq!(unix_seconds("2016-07-11T22:14:10.123Z"), Ok(1_468_275_250));
}

#[test]
fn expires_at_in_another_offset_or_malformed_is_refused_by_name() {
    for (time, why) in [
        ("2016-07-11T22:14:10+00:00", "UTC"),
        ("2016-07-11T15:14:10-07:00", "UTC"),
        ("2016-07-11T22:14:10", "no offset"),
        ("2016-07-11 22:14:10Z", "RFC 3339"),
        ("2023-02-29T00:00:00Z", "no such time"),
        ("2016-13-01T00:00:00Z", "no such month"),
        ("2016-07-11T24:00:00Z", "no such time"),
        ("2016-07-11T22:14:10.Z", "fraction"),
        ("1969-12-31T23:59:59Z", "no such time"),
        ("soon", "RFC 3339"),
    ] {
        let error = unix_seconds(time).unwrap_err();
        assert!(error.what.contains(time), "{}", error.what);
        assert!(error.what.contains(why), "{time}: {}", error.what);
    }
}

#[test]
fn a_bad_repository_or_host_is_refused_before_anything_is_loaded() {
    for repo in ["blaze", "/blaze", "owner/", "owner/../x", "owner/name/more", "owner/na me", "../x"] {
        let app = App { id: 1, installation: 2, key: test_key(), api: "api.github.com".to_owned() };
        let error = GitHub::new(app, repo.to_owned()).err().unwrap();
        assert!(error.what.contains(repo), "{}", error.what);
    }
    for api in ["", "api.github.com/evil", "api.github.com:8443", "user@api.github.com"] {
        let app = App { id: 1, installation: 2, key: test_key(), api: api.to_owned() };
        assert!(GitHub::new(app, "indexable-inc/blaze".to_owned()).is_err(), "{api}");
    }
}
