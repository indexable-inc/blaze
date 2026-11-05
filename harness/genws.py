#!/usr/bin/env python3
"""genws.py: a cargo workspace of any number of crates, made from a seed, and edits to it.

    genws.py DIR CRATES [--seed S] [--modules M]

The workspace is one program, `app`, and CRATES - 1 libraries in layers. `g000` is the bottom
layer: the trait `Scale`, the generic functions over it, `mix` and the `Trace` every crate
writes to. Every other library depends on `g000` and on up to three libraries of lower layers,
one of them of the layer just below, so the longest chain of dependencies is as long as there
are layers (`layers(CRATES)`). `app` depends on the top layer and on whatever nothing else
depends on, so that every crate is linked and run.

A library is M modules (12 unless told otherwise) of the same shape and different constants:
a struct with derives and methods, an enum, impls of `Scale` for both, functions over
`BTreeMap`, `Vec`, `String`, `Option` and `Result`, closures, calls into the modules of the
crates it depends on, and unit tests. `app` prints one line per crate and a total, which every
edit below except `signature` changes; it has unit tests and an integration test.

`edits(ws)` is the edits bench.py makes, in its form: lists of (files, old, new).
"""
import json, math, os, random, re, sys

BASE = "g000"
ANCHOR = "// genws: the end of the module\n"


def layers(crates):
    return max(2, math.ceil(math.log2(max(crates, 2))))


def plan(crates, seed):
    """The libraries by name, each with its layer and the libraries it depends on, and what
    `app` depends on."""
    rng = random.Random(f"{seed}-{crates}")
    count, depth = crates - 1, layers(crates)
    libs = {BASE: dict(layer=0, deps=[])}
    by_layer = {0: [BASE]}
    for at in range(1, count):
        layer = min(depth - 1, 1 + (at - 1) * (depth - 1) // max(count - 1, 1)) if depth > 1 else 0
        below = by_layer[max(k for k in by_layer if k < layer)]
        lower = [n for n, l in libs.items() if l["layer"] < layer and n != BASE]
        deps = {rng.choice(below)} | set(rng.sample(lower, min(2, len(lower))))
        name = f"g{at:03}"
        libs[name] = dict(layer=layer, deps=sorted(deps - {BASE}))
        by_layer.setdefault(layer, []).append(name)
    used = {d for l in libs.values() for d in l["deps"]}
    top = max(by_layer)
    roots = sorted(n for n, l in libs.items() if l["layer"] == top or (n not in used and n != BASE))
    return libs, roots


def module(rng, crate, index, deps):
    """The text of one module."""
    k = index
    c = [rng.randrange(3, 1 << 31) | 1 for _ in range(12)]
    r = [rng.randrange(1, 63) for _ in range(4)]
    ours = "crate" if crate == BASE else BASE
    calls = "".join(f"\n    acc = {ours}::mix(acc, {d}::m{rng.randrange(MODULES)}::step(x ^ {rng.randrange(99)}));"
                    f"\n    acc ^= {d}::m{rng.randrange(MODULES)}::Rec::new(x % 7).score();" for d in deps)
    return f"""//! Module {k} of `{crate}`.
use std::collections::BTreeMap;

use {ours}::Scale;

#[derive(Debug, Clone, PartialEq)]
pub struct Rec {{
    pub id: u64,
    pub name: String,
    pub tags: Vec<u32>,
    pub ratio: f64,
}}

impl Rec {{
    pub fn new(id: u64) -> Self {{
        let tags = (0..(id % 5 + 2) as u32).map(|t| t.wrapping_mul({c[0] % 1000}).wrapping_add({c[1] % 100})).collect();
        Self {{ id, name: format!("{crate}-m{k}-{{id}}"), tags, ratio: id as f64 * 0.5 + {k}.0 }}
    }}

    pub fn score(&self) -> u64 {{
        let mut acc = self.id ^ {c[2]};
        for tag in &self.tags {{
            acc = acc.wrapping_mul({c[3]}).wrapping_add(*tag as u64);
        }}
        acc ^ self.name.len() as u64 ^ (self.ratio as u64)
    }}

    pub fn label(&self) -> String {{
        format!("{{}}#{{}}[{{}}]", self.name, self.id, self.tags.len())
    }}

    pub fn bump(&mut self, by: u32) {{
        for tag in &mut self.tags {{
            *tag = tag.rotate_left({r[0] % 31 + 1}) ^ by;
        }}
        self.ratio += by as f64;
    }}

    pub fn merged(&self, other: &Self) -> Self {{
        let mut tags = self.tags.clone();
        tags.extend(other.tags.iter().copied());
        Self {{ id: self.id ^ other.id, name: format!("{{}}+{{}}", self.name, other.name), tags, ratio: self.ratio.max(other.ratio) }}
    }}
}}

impl Scale for Rec {{
    fn scale(&self) -> u64 {{
        self.score() ^ {c[4]}
    }}
}}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {{
    Small(u8),
    Wide {{ w: u32, h: u32 }},
    Named(u16),
    Empty,
}}

impl Kind {{
    pub fn of(x: u64) -> Self {{
        match x % 4 {{
            0 => Kind::Small(x as u8),
            1 => Kind::Wide {{ w: (x >> 3) as u32, h: (x & 0xff) as u32 }},
            2 => Kind::Named((x >> {r[1] % 16}) as u16),
            _ => Kind::Empty,
        }}
    }}

    pub fn area(self) -> u64 {{
        match self {{
            Kind::Small(v) => v as u64 + {c[5] % 1000},
            Kind::Wide {{ w, h }} => (w as u64).wrapping_mul(h as u64),
            Kind::Named(n) => (n as u64) << 2,
            Kind::Empty => {c[6] % 1000},
        }}
    }}
}}

impl Scale for Kind {{
    fn scale(&self) -> u64 {{
        self.area().rotate_left({r[2]})
    }}
}}

pub fn step(x: u64) -> u64 {{
    let salt = {c[7]}u64;
    x.wrapping_mul(salt).rotate_left({r[3]}) ^ {c[8]}
}}

pub fn table(n: u64) -> BTreeMap<u64, String> {{
    let mut map = BTreeMap::new();
    for i in 0..n {{
        map.insert(step(i) % 97, format!("v{{}}", i * {c[9] % 50 + 1}));
    }}
    map
}}

pub fn collect(n: u64) -> Vec<Rec> {{
    (0..n).map(|i| Rec::new(step(i) % 1000)).collect()
}}

pub fn summarize(items: &[Rec]) -> String {{
    let longest = items.iter().map(|it| it.label()).max_by_key(|l| l.len()).unwrap_or_default();
    let total: u64 = items.iter().filter(|it| it.id % 2 == 0).map(Rec::score).fold(0, u64::wrapping_add);
    format!("{{longest}}:{{total}}")
}}

pub fn parse(text: &str) -> Option<u64> {{
    let (head, tail) = text.split_once(':')?;
    let value: u64 = tail.trim().parse().ok()?;
    Some(value.wrapping_add(head.len() as u64 * {c[10] % 100}))
}}

pub fn checked(x: u64) -> Result<u64, String> {{
    let text = format!("k{k}: {{}}", x % 100_000);
    let value = parse(&text).ok_or_else(|| format!("no value in {{text}}"))?;
    if value % {c[11] % 13 + 2} == 0 {{
        return Err(format!("{{value}} is refused"));
    }}
    Ok(value)
}}

fn kinds(x: u64) -> Vec<Kind> {{
    let mut all: Vec<Kind> = (0..6).map(|i| Kind::of(step(x + i))).collect();
    all.sort();
    all
}}

fn folded(x: u64) -> u64 {{
    let recs = collect(x % 5 + 3);
    let mut first = recs[0].clone();
    first.bump(x as u32);
    let merged = first.merged(&recs[1]);
    {ours}::fold_all(&recs) ^ {ours}::fold_all(&kinds(x)) ^ {ours}::largest(&[merged, first]).map_or(0, |it| it.scale())
}}

pub fn uses(x: u64) -> u64 {{
    let mut acc = step(x);{calls}
    acc ^= x >> 3;
    acc
}}

pub fn digest(x: u64) -> u64 {{
    let map = table(x % 9 + 4);
    let text = summarize(&collect(x % 4 + 2));
    let mut acc = {ours}::mix(folded(x), uses(x));
    acc = {ours}::mix(acc, map.values().map(|v| v.len() as u64).sum());
    acc = {ours}::mix(acc, text.len() as u64);
    match checked(acc) {{
        Ok(value) => {ours}::mix(acc, value),
        Err(why) => {ours}::mix(acc, why.len() as u64),
    }}
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn digest_repeats() {{
        assert_eq!(digest({k + 1}), digest({k + 1}));
    }}

    #[test]
    fn parses() {{
        assert_eq!(parse("ab: 5"), Some(5 + 2 * {c[10] % 100}));
        assert_eq!(parse("none"), None);
    }}

    #[test]
    fn kinds_sort() {{
        let all = kinds(3);
        assert!(all.windows(2).all(|w| w[0] <= w[1]));
    }}
}}
{ANCHOR}"""


BASE_LIB = """
/// What every type the workspace folds over has.
pub trait Scale {
    fn scale(&self) -> u64;
}

impl Scale for u64 {
    fn scale(&self) -> u64 {
        *self
    }
}

impl Scale for String {
    fn scale(&self) -> u64 {
        self.len() as u64
    }
}

impl<T: Scale> Scale for Option<T> {
    fn scale(&self) -> u64 {
        self.as_ref().map_or(1, Scale::scale)
    }
}

impl<T: Scale> Scale for Vec<T> {
    fn scale(&self) -> u64 {
        fold_all(self)
    }
}

pub fn fold_all<T: Scale>(items: &[T]) -> u64 {
    let mut acc = 17u64;
    for item in items {
        acc = acc.wrapping_mul(31).wrapping_add(item.scale());
    }
    acc
}

pub fn largest<T: Scale + Clone>(items: &[T]) -> Option<T> {
    items.iter().max_by_key(|it| it.scale()).cloned()
}

pub fn mix(a: u64, b: u64) -> u64 {
    let turned = a.rotate_left(5) ^ b;
    turned.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// What the crates of the workspace have computed, each once.
pub struct Trace {
    seed: u64,
    seen: Vec<&'static str>,
    lines: Vec<String>,
    total: u64,
}

impl Trace {
    pub fn new(seed: u64) -> Self {
        Self { seed, seen: Vec::new(), lines: Vec::new(), total: 0 }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Whether `name` is here for the first time.
    pub fn enter(&mut self, name: &'static str) -> bool {
        if self.seen.contains(&name) {
            return false;
        }
        self.seen.push(name);
        true
    }

    pub fn record(&mut self, name: &str, value: u64) {
        self.total = crate::mix(self.total, value);
        self.lines.push(format!("{name} {value:016x}"));
    }

    pub fn finish(mut self) -> String {
        self.lines.sort();
        format!("{}\\ntotal {:016x} of {}", self.lines.join("\\n"), self.total, self.seen.len())
    }
}
"""


def library(rng, name, deps):
    """The files of one library by path."""
    mods = range(MODULES)
    all_deps = deps if name == BASE else [BASE, *deps]
    manifest = f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n' + "".join(f'{d} = {{ path = "../{d}" }}\n' for d in all_deps)
    trace = "crate::Trace" if name == BASE else f"{BASE}::Trace"
    lib = "".join(f"pub mod m{k};\n" for k in mods) + (BASE_LIB if name == BASE else "") + f"""
pub fn digest(x: u64) -> u64 {{
    {" ^ ".join(f"m{k}::digest(x + {k})" for k in mods)}
}}

pub fn run(trace: &mut {trace}) {{
    if !trace.enter("{name}") {{
        return;
    }}
    let value = digest(trace.seed());
    trace.record("{name}", value);
{"".join(f"    {d}::run(trace);{chr(10)}" for d in deps)}}}

#[cfg(test)]
mod tests {{
    #[test]
    fn digests_differ() {{
        assert_ne!(super::digest(1), super::digest(2));
    }}

    #[test]
    fn runs_once() {{
        let mut trace = {trace}::new(3);
        super::run(&mut trace);
        let first = trace.finish();
        assert!(first.contains("{name} "));
    }}
}}
"""
    files = {f"{name}/Cargo.toml": manifest, f"{name}/src/lib.rs": lib}
    for k in mods:
        files[f"{name}/src/m{k}.rs"] = module(rng, name, k, deps)
    return files


def app(roots):
    deps = sorted({BASE, *roots})
    return {
        "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n' + "".join(f'{d} = {{ path = "../{d}" }}\n' for d in deps),
        "app/src/main.rs": f"""fn salted(seed: u64) -> u64 {{
    let salt = 1u64;
    seed.wrapping_mul(1000).wrapping_add(salt)
}}

fn main() {{
    let seed = std::env::args().nth(1).and_then(|it| it.parse().ok()).unwrap_or(7);
    let mut trace = {BASE}::Trace::new(salted(seed));
{"".join(f"    {d}::run(&mut trace);{chr(10)}" for d in deps)}    println!("{{}}", trace.finish());
}}

#[cfg(test)]
mod tests {{
    #[test]
    fn salts() {{
        assert_eq!(super::salted(2) / 1000, 2);
    }}
}}
""",
        "app/tests/it.rs": f"""#[test]
fn the_program_prints_a_total() {{
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_app")).arg("5").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().lines().last().unwrap().starts_with("total "));
}}

#[test]
fn the_roots_run() {{
    let mut trace = {BASE}::Trace::new(11);
    {roots[0]}::run(&mut trace);
    assert!(trace.finish().contains("{roots[0]} "));
}}
""",
    }


def generate(ws, crates, seed=1, modules=12):
    """Writes the workspace. The result is what it is made of."""
    global MODULES
    MODULES = modules
    libs, roots = plan(crates, seed)
    rng = random.Random(f"{seed}-{crates}-text")
    files = {"Cargo.toml": '[workspace]\nmembers = ["app", ' + ", ".join(f'"{n}"' for n in libs) + ']\nresolver = "2"\n',
             ".gitignore": "target*\n"}
    for name, lib in libs.items():
        files.update(library(rng, name, lib["deps"]))
    files.update(app(roots))
    for path, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)) or ws, exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    dependents = {n: sum(n in l["deps"] for l in libs.values()) for n in libs}
    # The library the edits to "a crate in the middle" go to: of the middle layer, the one
    # most others depend on.
    middle = max((n for n, l in libs.items() if l["layer"] == max(1, layers(crates) // 2)) if crates > 2 else [BASE], key=lambda n: (dependents[n], n))
    rust = [t for p, t in files.items() if p.endswith(".rs")]
    made = dict(crates=crates, seed=seed, modules=modules, layers=1 + max(l["layer"] for l in libs.values()), roots=roots, middle=middle,
                libraries=sorted(libs), files=len(rust), lines=sum(t.count("\n") for t in rust),
                functions=sum(len(re.findall(r"\bfn \w+", t)) for t in rust), tests=sum(t.count("#[test]") for t in rust),
                dependents_of_middle=dependents[middle])
    json.dump(made, open(os.path.join(ws, "genws.json"), "w"), indent=1)
    return made


def edits(ws):
    """The edits to a workspace made here, by the name of their shape: lists of (files, old,
    new). `scattered(ws, n)` is the one over n crates."""
    made = json.load(open(os.path.join(ws, "genws.json")))
    mid = made["middle"]
    field = [
        (f"{mid}/src/m0.rs", "    pub ratio: f64,\n}", "    pub ratio: f64,\n    pub extra: u16,\n}"),
        (f"{mid}/src/m0.rs", "ratio: id as f64 * 0.5 + 0.0 }", "ratio: id as f64 * 0.5 + 0.0, extra: id as u16 ^ 9 }"),
        (f"{mid}/src/m0.rs", "ratio: self.ratio.max(other.ratio) }", "ratio: self.ratio.max(other.ratio), extra: self.extra }"),
        (f"{mid}/src/m0.rs", "acc ^ self.name.len() as u64 ^ (self.ratio as u64)", "acc ^ self.name.len() as u64 ^ (self.ratio as u64) ^ self.extra as u64"),
    ]
    return dict(
        body=[("app/src/main.rs", "let salt = 1u64;", "let salt = 2u64;")],
        deep=[(f"{BASE}/src/lib.rs", "let turned = a.rotate_left(5) ^ b;", "let turned = a.rotate_left(7) ^ b;")],
        signature=[
            (f"{BASE}/src/lib.rs", "pub fn mix(a: u64, b: u64) -> u64 {", "pub fn mix(_bench: (), a: u64, b: u64) -> u64 {"),
            (f"{BASE}/src/*.rs", "crate::mix(", "crate::mix((), "),
            ("g*/src/m*.rs", f"{BASE}::mix(", f"{BASE}::mix((), "),
        ],
        generic=[(f"{BASE}/src/lib.rs", "acc = acc.wrapping_mul(31).wrapping_add(item.scale());", "acc = acc.wrapping_mul(33).wrapping_add(item.scale());")],
        new_fn=[
            (f"{mid}/src/m0.rs", ANCHOR, ANCHOR + "\npub fn bench_added(x: u64) -> u64 {\n    let mut acc = x;\n    for i in 0..5 {\n        acc = step(acc ^ i);\n    }\n    acc\n}\n"),
            (f"{mid}/src/lib.rs", "m0::digest(x + 0)", "m0::digest(x + 0) ^ m0::bench_added(x)"),
        ],
        new_type=[
            (f"{mid}/src/m1.rs", ANCHOR, ANCHOR + f"\n#[derive(Debug, Clone, Default)]\npub struct BenchAdded {{\n    pub parts: Vec<u64>,\n    pub name: String,\n}}\n\nimpl BenchAdded {{\n    pub fn of(x: u64) -> Self {{\n        Self {{ parts: (0..4).map(|i| step(x + i)).collect(), name: format!(\"added-{{x}}\") }}\n    }}\n}}\n\nimpl {BASE}::Scale for BenchAdded {{\n    fn scale(&self) -> u64 {{\n        {BASE}::fold_all(&self.parts) ^ self.name.len() as u64\n    }}\n}}\n"),
            (f"{mid}/src/lib.rs", "m1::digest(x + 1)", f"m1::digest(x + 1) ^ {BASE}::fold_all(&[m1::BenchAdded::of(x)])"),
        ],
        field=field,
        middle=[(f"{mid}/src/m0.rs", "let mut acc = step(x);", "let mut acc = step(x ^ 5);")],
    )


def scattered(ws, n):
    """An edit to one body in each of `n` libraries, spread over the layers."""
    made = json.load(open(os.path.join(ws, "genws.json")))
    libs = made["libraries"]
    if n > len(libs) + 1:
        return None
    # With as many edits as there are crates, the program's own is one of them.
    own = [("app/src/main.rs", "wrapping_mul(1000)", "wrapping_mul(1001)")] if n > len(libs) else []
    n -= len(own)
    chosen = [libs[i * len(libs) // n] for i in range(n)]
    return own + [(f"{name}/src/m{i % made['modules']}.rs", ".rotate_left(", ".rotate_left(1 + ") for i, name in enumerate(chosen)]

if __name__ == "__main__":
    argv = sys.argv[1:]
    seed = int(argv[argv.index("--seed") + 1]) if "--seed" in argv else 1
    modules = int(argv[argv.index("--modules") + 1]) if "--modules" in argv else 12
    print(json.dumps(generate(argv[0], int(argv[1]), seed, modules)))
