//! Text-based dylib stubs (`.tbd`, the YAML subset Apple's SDKs ship: `--- !tapi-tbd` v4,
//! `!tapi-tbd-v3`, `!tapi-tbd-v2`), read as the linker reads one on its link line:
//! `blaze_link::fuzz_read(Format::MachO, arch, NativeKind::Shared { as_needed }, bytes)`, for
//! x86-64 and AArch64 on macOS (the platform `fuzz_read`'s output has). Text that is UTF-8 is
//! also read by `blaze_link::fuzz_tbd_differential` for each arch and platforms 1, 2 and 6
//! (macOS, iOS, Mac Catalyst), against the reference reader `tbd::tests` keeps.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a slice of the text at a byte
//!   that is no char boundary panics too);
//! - the reader and the reference agree: the same symbols, inputs and install name, or the
//!   same error, and the reader's name arena holds exactly what it read;
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned;
//! - `as_needed` changes nothing the read accepts.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};

const TARGET: Target = Target {
    name: "link_macho_tbd",
    check,
    seeds,
    dictionary: &[
        b"--- !tapi-tbd\n",
        b"--- !tapi-tbd-v3\n",
        b"--- !tapi-tbd-v2\n",
        b"%YAML 1.2\n",
        b"\n...\n",
        b"tbd-version: 4\n",
        b"targets: [ ",
        b"archs: [ ",
        b"x86_64-macos",
        b"arm64-macos",
        b"arm64e-macos",
        b"arm64-ios",
        b"platform: macosx\n",
        b"install-name: ",
        b"current-version: ",
        b"exports:\n",
        b"  - targets: ",
        b"    symbols: [ ",
        b"objc-classes: ",
        b"objc-eh-types: ",
        b"objc-ivars: ",
        b"weak-symbols: ",
        b"weak-def-symbols: ",
        b"thread-local-symbols: ",
        b"reexports:\n",
        b"re-exports: ",
        b"reexported-libraries:\n",
        b"parent-umbrella:\n",
        b"uuids: ",
        b"libraries: ",
        b" ]\n",
        b"'",
        b"''",
        b"\"",
        b"\\x41",
        b"# ",
        b"\xc3\xa9",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const SHARED: NativeKind = NativeKind::Shared { as_needed: false };
const AS_NEEDED: NativeKind = NativeKind::Shared { as_needed: true };
const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];
/// The `.tbd` platforms the reference reads: macOS, iOS and Mac Catalyst.
const PLATFORMS: [u32; 3] = [1, 2, 6];

/// An arch, the read compared with its first (shifted, `as_needed`) and the platform the
/// text is read against the reference for, one of each a run (`blaze_fuzzer::case`), every
/// combination some case. The plain test takes every case of each seed.
const CASES: usize = ARCHES.len() * 2 * PLATFORMS.len();

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CASES));
}

fn check_case(data: &[u8], case: usize) {
    let arch = ARCHES[case % ARCHES.len()];
    let (compared, platform) = (case / ARCHES.len() % 2, PLATFORMS[case / ARCHES.len() / 2]);
    let (aligned, at) = copy_at(data, 0);
    let aligned = &aligned[at..][..data.len()];
    let bound = blaze_fuzz::allowed(data.len());
    let (largest, read) =
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::MachO, arch, SHARED, aligned));
    assert!(
        largest <= bound,
        "allocation bound: a {arch:?} read of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
    if compared == 0 {
        let (shifted, at) = copy_at(data, 1);
        let shifted = &shifted[at..][..data.len()];
        assert_eq!(
            blaze_link::fuzz_read(Format::MachO, arch, SHARED, shifted),
            read,
            "alignment: a {arch:?} read of the bytes one past an 8-aligned address answers otherwise"
        );
    } else {
        assert_eq!(
            blaze_link::fuzz_read(Format::MachO, arch, AS_NEEDED, aligned),
            read,
            "as needed: a {arch:?} read with `as_needed` answers otherwise"
        );
    }
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Err(e) = blaze_link::fuzz_tbd_differential(text, arch, platform) {
        panic!("differential: a {arch:?} read for platform {platform}: {e}");
    }
}

/// `data` copied to `shift` past an 8-aligned address, and where it starts in the copy.
fn copy_at(data: &[u8], shift: usize) -> (Vec<u8>, usize) {
    let mut buffer = vec![0xff; data.len() + 16];
    let start = (8 - buffer.as_ptr() as usize % 8) % 8 + shift;
    buffer[start..start + data.len()].copy_from_slice(data);
    (buffer, start)
}

/// The named texts, `tbd::tests::edges_match_reference`'s, and texts made as
/// `differential_seeded` makes them (its generator and seed).
fn seeds() -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = named().into_iter().map(|(_, _, text)| text.as_bytes().to_vec()).collect();
    out.extend(EDGES.iter().map(|text| text.as_bytes().to_vec()));
    let mut rng = Rng(0x7bd5_eed);
    for _ in 0..64 {
        out.push(generate(&mut rng).into_bytes());
    }
    out
}

/// Texts where a scalar wraps a line or the lexer's fast paths end.
const EDGES: &[&str] = &[
    "k: [ _a\n     _b, _c ]\n",
    "k: [ _a # c\n     _b ]\n",
    "k: [ _a,\n     ]\n",
    "k: [ 'a''\n     b' ]\n",
    "k: [ a 'b, 'c\n d' ]\n",
    "k: { a: [ b ], 'c': , d }\n",
    "k: \"\\x4\n1\"\n",
    "k: \"\\u00e9\\U0001F600\\0\\t\\/\\ \"\n",
    "k: \"\\q\"\n",
    "k: \"\\xZZ\"\n",
    "k: \"\\uD800\"\n",
    "k: \"\\x4é\"\n",
    "'a\n",
    "k: [ a ] b\n",
    "k: v\n  x: y\n",
    "- a\n-\n  - b\n- c: d\n  e: f\n",
    "a: 1\na: 2\n",
    "a:\n- x\n- y\nb: { }\n",
    "\tk: v\n",
    "k: ]\n",
    "a b: c\nfoo [ x,\n y: ] : z\n",
    "\"k\"   :   v\n'k2':v\n",
    "%YAML 1.2\n--- !tapi-tbd # c\na: b\n...\nloose: line\n---\n...\n",
    "a: b\r\nc: [ d,\r\n  e ]\r\n",
    "a: 'x' # c\nb: \"y\" #c\n",
    "a: x#y\nb: '#'\n",
    "a: [ \"x\\\"\", 'y''' ]\n",
    "a: { k: { j: [ [ ], [ x ] ] } }\n",
    "- - - a\n",
    "a: *b\n'c': \x02\x03\x07\n",
];

/// `tbd::tests`' generator: splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }

    fn subset<'a>(&mut self, items: &[&'a str]) -> Vec<&'a str> {
        let mut out: Vec<&str> = items.iter().copied().filter(|_| self.one_in(2)).collect();
        if out.is_empty() {
            out.push(self.pick(items));
        }
        out
    }
}

const TARGETS: &[&str] = &[
    "x86_64-macos",
    "arm64-macos",
    "arm64e-macos",
    "arm64-ios",
    "x86_64-maccatalyst",
    "arm64e.x1-macos",
    "x86_64h-macos",
];
const GEN_ARCHS: &[&str] = &["x86_64", "arm64", "arm64e", "x86_64h"];
const NAMES: &[&str] = &[
    "_malloc",
    "_free",
    "_printf",
    "dyld_stub_binder",
    "_OBJC_CLASS_$_NSObject",
    "$ld$hide$os10.4$_foo",
    "_it's",
    "_a b",
    "_a:b",
    "_x#y",
    "_c,d",
    "_[e]",
    "__dup",
    "_é",
    "NSFoo",
    "_Old",
    "Bar._x",
    "_tlv",
    "_q\"uote",
    "_back\\slash",
];

/// `name` as a scalar: plain when it can be, else quoted one way or the other, with escapes
/// now and then.
fn render(rng: &mut Rng, name: &str) -> String {
    let plain = !name.is_empty() && !name.contains([',', '[', ']', '{', '}', '#', '\'', '"', ':', ' ', '\\']);
    match rng.below(if plain { 4 } else { 2 }) {
        0 => format!("'{}'", name.replace('\'', "''")),
        1 => {
            let mut out = String::from("\"");
            for c in name.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    'a' if rng.one_in(2) => out.push_str("\\x61"),
                    'é' if rng.one_in(2) => out.push_str("\\u00e9"),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        _ => name.to_owned(),
    }
}

/// `key: <items>` at `indent`: a flow list on one line or wrapped (with comments now and
/// then), or a block sequence, nested or not.
fn list(rng: &mut Rng, out: &mut String, nl: &str, indent: usize, key: &str, items: &[String]) {
    let pad = " ".repeat(indent);
    if !items.is_empty() && rng.one_in(4) {
        let deeper = if rng.one_in(2) { indent + 2 } else { indent };
        out.push_str(&format!("{pad}{key}:{nl}"));
        for item in items {
            out.push_str(&format!("{}- {item}{nl}", " ".repeat(deeper)));
        }
        return;
    }
    out.push_str(&format!("{pad}{key}:{}[ ", " ".repeat(1 + rng.below(6))));
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
            if rng.one_in(3) {
                if rng.one_in(3) {
                    out.push_str(" # wrapped");
                }
                out.push_str(nl);
                out.push_str(&" ".repeat(indent + 4));
            } else {
                out.push(' ');
            }
        }
        out.push_str(item);
    }
    out.push_str(&format!(" ]{nl}"));
}

fn library(index: usize) -> String {
    format!("/usr/lib/lib{index}.dylib")
}

fn names_for(rng: &mut Rng) -> Vec<String> {
    (0..rng.below(6))
        .map(|_| {
            let name = NAMES[rng.below(NAMES.len())];
            render(rng, name)
        })
        .collect()
}

fn libraries_for(rng: &mut Rng, most: usize, count: usize) -> Vec<String> {
    (0..1 + rng.below(most))
        .map(|_| {
            let name = library(rng.below(count + 1));
            render(rng, &name)
        })
        .collect()
}

fn doc_v4(rng: &mut Rng, out: &mut String, nl: &str, index: usize, count: usize) {
    let targets = rng.subset(TARGETS);
    let plain: Vec<String> = targets.iter().map(|it| (*it).to_owned()).collect();
    out.push_str(&format!("--- !tapi-tbd{nl}tbd-version:     4{nl}"));
    list(rng, out, nl, 0, "targets", &plain);
    if rng.one_in(3) {
        out.push_str(&format!("# a comment{nl}{nl}"));
    }
    out.push_str(&format!("install-name:    {}{nl}", render(rng, &library(index))));
    if rng.one_in(2) {
        out.push_str(&format!("current-version: 1.2{nl}"));
    }
    if rng.one_in(3) {
        out.push_str(&format!(
            "parent-umbrella:{nl}  - targets:         [ {} ]{nl}    umbrella:        System{nl}",
            targets.join(", ")
        ));
    }
    if rng.one_in(2) {
        out.push_str(&format!("reexported-libraries:{nl}"));
        out.push_str(&format!("  - targets:         [ {} ]{nl}", rng.subset(&targets).join(", ")));
        let libraries = libraries_for(rng, 3, count);
        list(rng, out, nl, 4, "libraries", &libraries);
    }
    for key in ["exports", "reexports"] {
        if rng.one_in(4) {
            continue;
        }
        out.push_str(&format!("{key}:{nl}"));
        for _ in 0..1 + rng.below(3) {
            out.push_str(&format!("  - targets:         [ {} ]{nl}", rng.subset(&targets).join(", ")));
            for list_key in
                ["symbols", "weak-symbols", "thread-local-symbols", "objc-classes", "objc-eh-types", "objc-ivars"]
            {
                if rng.one_in(2) {
                    let items = names_for(rng);
                    list(rng, out, nl, 4, list_key, &items);
                }
            }
            if rng.one_in(6) {
                out.push_str(&format!("    allowable-clients: [ foo ]{nl}"));
            }
        }
    }
}

fn doc_v3(rng: &mut Rng, out: &mut String, nl: &str, index: usize, count: usize) {
    let tag = rng.pick(&["--- !tapi-tbd-v3", "--- !tapi-tbd-v2", "---"]);
    let archs = rng.subset(GEN_ARCHS);
    out.push_str(&format!("{tag}{nl}archs:           [ {} ]{nl}", archs.join(", ")));
    if rng.one_in(2) {
        let uuids: Vec<String> = archs.iter().map(|it| format!("'{it}: 0000-1111'")).collect();
        list(rng, out, nl, 0, "uuids", &uuids);
    }
    out.push_str(&format!("platform:        {}{nl}", rng.pick(&["macosx", "ios", "iosmac", "zippered"])));
    out.push_str(&format!("install-name:    {}{nl}", render(rng, &library(index))));
    out.push_str(&format!("exports:{nl}"));
    for _ in 0..1 + rng.below(3) {
        out.push_str(&format!("  - archs:           [ {} ]{nl}", rng.subset(&archs).join(", ")));
        if rng.one_in(2) {
            let libraries = libraries_for(rng, 2, count);
            list(rng, out, nl, 4, "re-exports", &libraries);
        }
        for list_key in ["symbols", "weak-def-symbols", "thread-local-symbols", "objc-classes", "objc-eh-types", "objc-ivars"]
        {
            if rng.one_in(2) {
                let items = names_for(rng);
                list(rng, out, nl, 4, list_key, &items);
            }
        }
    }
}

/// Versions 2 to 4, several documents, re-exports in and out of the file, quoting and
/// escapes, wrapped and block lists, comments, CRLF, and now and then a byte put in or taken
/// out for the error paths.
fn generate(rng: &mut Rng) -> String {
    let nl = if rng.one_in(4) { "\r\n" } else { "\n" };
    let mut out = String::new();
    if rng.one_in(5) {
        out.push_str(&format!("%YAML 1.2{nl}"));
    }
    let count = 1 + rng.below(3);
    for index in 0..count {
        if rng.one_in(3) {
            doc_v3(rng, &mut out, nl, index, count);
        } else {
            doc_v4(rng, &mut out, nl, index, count);
        }
        if rng.one_in(4) {
            out.push_str(nl);
        }
        if !rng.one_in(5) {
            out.push_str(&format!("...{nl}"));
        }
    }
    if rng.one_in(2) {
        for _ in 0..1 + rng.below(3) {
            let boundaries: Vec<usize> = (0..=out.len()).filter(|&it| out.is_char_boundary(it)).collect();
            let at = boundaries[rng.below(boundaries.len())];
            if rng.one_in(2) {
                const BYTES: &[u8] = b"[]{}'\",:#- \n\tx";
                let byte = BYTES[rng.below(BYTES.len())];
                out.insert(at, char::from(byte));
            } else if at < out.len() {
                out.remove(at);
            }
        }
    }
    out
}

fn named() -> Vec<(&'static str, Arch, &'static str)> {
    vec![
        ("v4 of two documents, its AArch64 slice", Arch::Aarch64, V4),
        ("v4 of two documents, its x86-64 slice", Arch::X86_64, V4),
        ("v4 where the exact arch wins over arm64e", Arch::Aarch64, EXACT),
        ("v3 of two documents", Arch::Aarch64, V3),
        ("v2 with an objc class", Arch::X86_64, V2),
        ("a %YAML header and a comment after the tag", Arch::Aarch64, HEADER),
    ]
}

const V4: &str = "\
--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64-macos,
                   arm64-ios ]
install-name:    '/usr/lib/libfoo.dylib'
current-version: 1.2
# a comment line
reexported-libraries:
  - targets:         [ x86_64-macos, arm64-macos ]
    libraries:       [ '/usr/lib/system/libbar.dylib',
                       \"/usr/lib/libmissing.dylib\" ]
exports:
  - targets:         [ x86_64-macos, arm64-macos ]   # both
    symbols:         [ _foo, '_quoted$name', 'it''s',
                       _bar ]
    objc-classes:    [ NSFoo ]
    weak-symbols:    [ _weakling ]
    thread-local-symbols: [ _tlv ]
  - targets:         [ arm64-ios ]
    symbols:         [ _ios_only ]
  - targets:         [ x86_64-macos ]
    symbols:         [ _intel_only ]
reexports:
  - targets:         [ arm64-macos ]
    symbols:         [ _reexported_sym ]
...
--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64e-macos ]
install-name:    '/usr/lib/system/libbar.dylib'
parent-umbrella:
  - targets:         [ x86_64-macos, arm64e-macos ]
    umbrella:        foo
exports:
  - targets:         [ x86_64-macos, arm64e-macos ]
    symbols:         [ _from_bar, _foo ]
    objc-eh-types:   [ BarErr ]
    objc-ivars:      [ Bar._x ]
...
";

const EXACT: &str = "\
--- !tapi-tbd
tbd-version: 4
targets: [ arm64-macos, arm64e-macos ]
install-name: /usr/lib/libexact.dylib
exports:
  - targets: [ arm64-macos ]
    symbols: [ _plain ]
  - targets: [ arm64e-macos ]
    symbols: [ _authenticated ]
...
";

const V3: &str = "\
--- !tapi-tbd-v3
archs:           [ x86_64, arm64 ]
uuids:           [ 'x86_64: 00000000-0000-0000-0000-000000000000',
                   'arm64: 11111111-1111-1111-1111-111111111111' ]
platform:        macosx
install-name:    /usr/lib/libv3.dylib
current-version: 1
exports:
  - archs:           [ x86_64, arm64 ]
    re-exports:      [ /usr/lib/libv3sub.dylib ]
    symbols:         [ _common, \"_double\\x41\" ]
    objc-classes:    [ V3Class ]
    objc-ivars:      [ V3Class.ivar ]
    weak-def-symbols: [ _weak3 ]
    thread-local-symbols: [ _tls3 ]
  - archs:           [ x86_64 ]
    symbols:         [ _x86_only ]
...
--- !tapi-tbd-v3
archs:           [ x86_64, arm64 ]
platform:        macosx
install-name:    /usr/lib/libv3sub.dylib
exports:
  - archs:           [ arm64 ]
    symbols:         [ _sub_arm ]
...
";

const V2: &str = "\
--- !tapi-tbd-v2
archs: [ x86_64 ]
platform: macosx
install-name: /usr/lib/libv2.dylib
exports:
  - archs: [ x86_64 ]
    objc-classes: [ _Old ]
...
";

const HEADER: &str = "\
%YAML 1.2
--- !tapi-tbd # a comment
tbd-version: 4
targets: [ arm64-macos ]
install-name: /usr/lib/libheader.dylib
exports:
  - targets: [ arm64-macos ]
    symbols: [ _h ]
...
";

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_link::Format;

    use super::{SHARED, named};

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::CASES, super::check_case);
    }

    #[test]
    fn seeds_read() {
        for (what, arch, text) in named() {
            assert!(
                blaze_link::fuzz_read(Format::MachO, arch, SHARED, text.as_bytes()),
                "seed `{what}` is refused"
            );
            assert_eq!(blaze_link::fuzz_tbd_differential(text, arch, 1), Ok(true), "seed `{what}`, differential");
        }
    }
}
