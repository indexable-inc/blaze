//! Parses a Cargo manifest (blaze's, 9.6 KB) 10 times with basic-toml at run time and, with the
//! feature `const`, in a constant too, which the program checks against what it computes.

const MANIFEST: &str = include_str!("../manifest.toml");

fn entries(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => map.len() + map.values().map(entries).sum::<usize>(),
        serde_json::Value::Array(items) => items.len() + items.iter().map(entries).sum::<usize>(),
        _ => 0,
    }
}

fn parse() -> usize {
    let mut total = 0;
    for _ in 0..10 {
        match basic_toml::from_str::<serde_json::Value>(MANIFEST) {
            Ok(value) => total += entries(&value),
            Err(_) => return 0,
        }
    }
    total
}

#[cfg(feature = "const")]
const AT_COMPILE_TIME: usize = parse();

fn main() {
    let at_run_time = parse();
    #[cfg(feature = "const")]
    assert_eq!(AT_COMPILE_TIME, at_run_time, "the constant is not what the program computes");
    assert!(at_run_time > 0, "the manifest did not parse");
    println!("parsed 10 manifests: {at_run_time} entries");
}
