use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Crate {
    name: String,
    version: (u32, u32, u32),
    yanked: bool,
    total_downloads: Option<u64>,
    features: BTreeMap<String, Vec<String>>,
    kind: Kind,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Kind {
    Lib,
    Bin { path: String },
}

fn sample() -> Crate {
    Crate {
        name: "blaze".to_string(),
        version: (0, 20261028, 0),
        yanked: false,
        total_downloads: None,
        features: BTreeMap::from([("default".to_string(), vec!["std".to_string()]), ("std".to_string(), vec![])]),
        kind: Kind::Bin { path: "src/main.rs".to_string() },
    }
}

fn main() {
    let value = sample();
    let text = serde_json::to_string(&value).unwrap();
    println!("{text}");
    let back: Crate = serde_json::from_str(&text).unwrap();
    println!("round trip {}", back == value);

    let parsed: serde_json::Value =
        serde_json::from_str(r#"{"a": [1, 2.5, "x", null, true], "b": {"c": -3}}"#).unwrap();
    println!("{}", parsed["a"][1]);
    println!("{}", parsed["b"]["c"].as_i64().unwrap());
    println!("{}", serde_json::to_string_pretty(&parsed["a"]).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_kind() {
        let kind: Kind = serde_json::from_str(r#"{"type": "lib"}"#).unwrap();
        assert_eq!(kind, Kind::Lib);
    }

    #[test]
    fn refuses_a_missing_field() {
        assert!(serde_json::from_str::<Crate>(r#"{"name": "x"}"#).is_err());
    }
}
