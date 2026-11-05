//! A large JSON corpus through serde_json and simd-json and back: generated records into
//! typed structs, and every `.json` file under the directories named, as untyped values.

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct Record {
    id: u64,
    name: String,
    tags: Vec<String>,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<u64>,
    kind: Kind,
    attributes: BTreeMap<String, Attribute>,
    #[serde(rename = "nested-points")]
    points: Vec<(i32, i32)>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Kind {
    Plain,
    Weighted { weight: f32 },
    Linked { to: Vec<u64>, label: Option<String> },
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
#[serde(untagged)]
enum Attribute {
    Flag(bool),
    Count(i64),
    Ratio(f64),
    Text(String),
    Many(Vec<Attribute>),
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn word(&mut self) -> String {
        const PARTS: &[&str] = &["ka", "to", "\u{e9}", "mi", "\"q\"", "ru", "\\n", "\u{1f980}", "zo", "\t"];
        (0..2 + self.next() % 4).map(|_| PARTS[self.next() as usize % PARTS.len()]).collect()
    }

    fn attribute(&mut self, depth: u32) -> Attribute {
        match self.next() % if depth > 2 { 4 } else { 5 } {
            0 => Attribute::Flag(self.next() % 2 == 0),
            1 => Attribute::Count(self.next() as i64 - (1 << 30)),
            2 => Attribute::Ratio(self.next() as f64 / 1024.0 + 0.5),
            3 => Attribute::Text(self.word()),
            _ => Attribute::Many((0..self.next() % 4).map(|_| self.attribute(depth + 1)).collect()),
        }
    }

    fn record(&mut self, id: u64) -> Record {
        let kind = match self.next() % 3 {
            0 => Kind::Plain,
            1 => Kind::Weighted { weight: self.next() as f32 / 7.0 },
            _ => Kind::Linked {
                to: (0..self.next() % 5).map(|_| self.next() % (id + 1)).collect(),
                label: (self.next() % 2 == 0).then(|| self.word()),
            },
        };
        Record {
            id,
            name: self.word(),
            tags: (0..self.next() % 6).map(|_| self.word()).collect(),
            score: self.next() as f64 / 3.0,
            parent: (self.next() % 3 != 0).then(|| self.next() % (id + 1)),
            kind,
            attributes: (0..self.next() % 5).map(|_| (self.word(), self.attribute(0))).collect(),
            points: (0..self.next() % 4).map(|_| (self.next() as i32, -(self.next() as i32))).collect(),
        }
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3))
}

/// The records through both parsers and both printers; every form must agree.
fn generated(count: u64) {
    let mut lcg = Lcg(42);
    let records: Vec<Record> = (0..count).map(|id| lcg.record(id)).collect();
    let compact = serde_json::to_vec(&records).expect("records print");
    let pretty = serde_json::to_vec_pretty(&records).expect("records print");
    println!("generated {} records, {} bytes compact {:x}, {} pretty {:x}", count, compact.len(), fnv(&compact), pretty.len(), fnv(&pretty));
    let back: Vec<Record> = serde_json::from_slice(&pretty).expect("serde_json reads its own");
    println!("serde_json typed round trip: {}", back == records);
    let mut scratch = compact.clone();
    let simd: Vec<Record> = simd_json::serde::from_slice(&mut scratch).expect("simd-json reads serde_json's");
    println!("simd-json typed round trip: {}", simd == records);
    let again = simd_json::serde::to_vec(&simd).expect("simd-json prints");
    let reread: Vec<Record> = serde_json::from_slice(&again).expect("serde_json reads simd-json's");
    println!("simd-json printed {} bytes {:x}, read back equal: {}", again.len(), fnv(&again), reread == records);
    let value: serde_json::Value = serde_json::from_slice(&compact).expect("a value");
    let total: f64 = value.as_array().into_iter().flatten().filter_map(|it| it["score"].as_f64()).sum();
    println!("scores {total:.3}");
}

fn json_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            json_files(&path, out);
        } else if path.extension().is_some_and(|it| it == "json") {
            out.push(path);
        }
    }
}

/// Every JSON file under `dir` as a value of each library: what one prints the other reads.
fn corpus(dir: &Path) {
    let mut files = Vec::new();
    json_files(dir, &mut files);
    files.sort();
    let (mut bytes, mut hash, mut nodes, mut bad) = (0usize, 0u64, 0usize, 0usize);
    for path in &files {
        let Ok(text) = std::fs::read(path) else { continue };
        bytes += text.len();
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&text) else {
            bad += 1;
            continue;
        };
        let mut scratch = text.clone();
        let owned = simd_json::to_owned_value(&mut scratch).expect("simd-json reads what serde_json does");
        let printed = simd_json::to_vec(&owned).expect("simd-json prints");
        let back: serde_json::Value = serde_json::from_slice(&printed).expect("serde_json reads simd-json's");
        assert_eq!(back, value, "{}", path.display());
        nodes += count(&value);
        hash ^= fnv(&serde_json::to_vec(&value).expect("a value prints")).rotate_left(nodes as u32 % 64);
    }
    println!("corpus {}: {} files, {bytes} bytes, {nodes} nodes, {bad} not JSON, {hash:x}", dir.display(), files.len());
}

fn count(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(items) => 1 + items.iter().map(count).sum::<usize>(),
        serde_json::Value::Object(fields) => 1 + fields.values().map(count).sum::<usize>(),
        _ => 1,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let records = args.next().and_then(|it| it.parse().ok()).unwrap_or(20_000);
    generated(records);
    for dir in args {
        corpus(Path::new(&dir));
    }
    for broken in ["{\"a\": [1, 2,, 3]}", "[1e999999]", "\"\\ud800\"", "{\"a\":1 \"b\":2}"] {
        let serde = serde_json::from_str::<serde_json::Value>(broken).map_err(|e| e.to_string());
        let simd = simd_json::to_owned_value(&mut broken.as_bytes().to_vec()).is_ok();
        println!("{broken:?}: serde_json {serde:?}, simd-json ok {simd}");
    }
}
