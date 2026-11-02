use serde_json::{Value, json};

use super::*;
use crate::{ApiError, SCHEMA};

#[test]
fn a_response_is_one_line_of_its_envelope() {
    let envelope = Envelope::new(
        7,
        Response::Error(ApiError::new("two\nlines", Some("src/lib.rs".to_owned()))),
    );
    let line = response(&envelope);
    assert!(!line.contains('\n'), "{line}");
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        value,
        json!({
            "schema": SCHEMA,
            "id": 7,
            "body": { "Error": { "what": "two\nlines", "at": "src/lib.rs" } },
        })
    );
}

#[test]
fn a_request_is_one_line_of_its_envelope() {
    let line = request(&Envelope::new(3, Request::Cancel { id: 2 }));
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value, json!({ "schema": SCHEMA, "id": 3, "body": { "Cancel": { "id": 2 } } }));
    assert_eq!(request(&Envelope::new(1, Request::Status)), format!(
        r#"{{"schema":{SCHEMA},"id":1,"body":"Status"}}"#
    ));
}

#[test]
fn done_renders_its_outcome() {
    let line = response(&Envelope::new(9, Response::Done { ok: false }));
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["body"], json!({ "Done": { "ok": false } }));
}

/// Every character that is escaped, and those around them, is written as serde_json writes it,
/// alone and in runs long enough for every step of a SIMD escape: at its start, its end and
/// across one step into the next.
#[test]
fn escapes_are_the_bytes_serde_json_wrote() {
    let every: String = (0u8..0x80).map(char::from).chain(['\u{e9}', '\u{2028}', '\u{1f525}']).collect();
    let mut texts = vec![every.clone()];
    for c in every.chars() {
        for len in [1, 15, 16, 17, 31, 32, 33, 64] {
            for at in [0, len / 2, len - 1] {
                let mut text: String = "a".repeat(len);
                text.replace_range(at..at + 1, c.encode_utf8(&mut [0; 4]));
                texts.push(text);
            }
        }
    }
    for text in texts {
        let envelope = Envelope::new(1, Response::Error(ApiError::new(text, None)));
        assert_eq!(response(&envelope), serde_json::to_string(&envelope).unwrap());
    }
}
