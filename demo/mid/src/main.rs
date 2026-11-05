use serde_json::Value;

fn main() {
    for pattern in [r"a+b*", r"(?i)foo|bar\d{2,5}", r"[a-z&&[^aeiou]]+\b", r"\p{Greek}+", r"(unclosed", r"(?P<y>\d{4})-(?P<m>\d{2})"] {
        match regex_syntax::Parser::new().parse(pattern) {
            Ok(hir) => println!("{pattern} => {hir} / {:?}", hir.properties().minimum_len()),
            Err(err) => println!("{pattern} => error: {err}"),
        }
    }
    let text = r#"{"name": "blaze", "tags": ["a", "b\n", 1.5, -3, null, true], "nested": {"k": [1e10, 0.1, 18446744073709551615]}}"#;
    let value: Value = serde_json::from_str(text).unwrap();
    println!("{}", value);
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
    println!("{:?}", value["nested"]["k"][1].as_f64());
    println!("{:?}", serde_json::from_str::<Value>("{\"a\": [1, 2,}").map_err(|e| e.to_string()));
    let hay = b"the quick brown fox jumps over the lazy dog";
    println!("{:?} {:?} {:?}", memchr::memchr(b'q', hay), memchr::memrchr(b'o', hay), memchr::memmem::find(hay, b"lazy"));
    println!("{}", memchr::memchr_iter(b'o', hay).count());
}
