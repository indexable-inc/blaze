// blaze: the pin of blaze-stencil's libraries, `stencils.json` beside this file, as
// crates/hir-ty-codegen/llvm.json pins LLVM: each library's file and the id it must have, the
// BLAKE3 hash of its bytes with the id field zeroed (`format::Writer::finish`). blaze-stencil's
// build script holds every library it includes to it: a library the pinned LLVM makes (the
// `llvm-stencils` feature) must be the pinned one, so a change to what the generator makes is a
// change to the pin in the same tree; and the libraries committed in `prebuilt/` beside this
// file, which the seed profile (no LLVM) includes, must be the pinned ones in every build.
// blaze-codegen-code includes this file to key the code identity by the pin. Std only and no
// `use`, so it includes into any build script.

/// The pin, in blaze-stencil's directory.
pub(crate) const PIN_FILE: &str = "stencils.json";
/// The directory, in blaze-stencil's, of the pinned libraries as committed data
/// (docs/future/bootstrap.md, "Who owns stencil generation"): the seed profile, which has no
/// LLVM, includes these bytes; a build with LLVM checks that what it made is them.
pub(crate) const PREBUILT: &str = "prebuilt";

/// Each library's file and its pinned id, in the pin's order. `json` is one object of
/// `"<file>": "<64 lowercase hex digits>"`, each file named once, its strings with no escape.
pub(crate) fn pin_parse(json: &str) -> Result<Vec<(String, [u8; 32])>, String> {
    let mut rest = json.trim().strip_prefix('{').ok_or("the pin is no JSON object")?.trim_start();
    let mut out: Vec<(String, [u8; 32])> = Vec::new();
    if let Some(after) = rest.strip_prefix('}') {
        return if after.trim().is_empty() {
            Ok(out)
        } else {
            Err("text after the pin's object".to_owned())
        };
    }
    loop {
        let (name, after) = pin_string(rest)?;
        rest = after
            .trim_start()
            .strip_prefix(':')
            .ok_or_else(|| format!("\"{name}\" has no `:`"))?
            .trim_start();
        let (hex, after) = pin_string(rest)?;
        let id = pin_unhex(&hex)
            .ok_or_else(|| format!("\"{name}\" is \"{hex}\", not 64 lowercase hex digits"))?;
        if out.iter().any(|(it, _)| *it == name) {
            return Err(format!("\"{name}\" is pinned twice"));
        }
        out.push((name, id));
        rest = after.trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
            continue;
        }
        let after = rest.strip_prefix('}').ok_or("a pinned id is followed by no `,` or `}`")?;
        if !after.trim().is_empty() {
            return Err("text after the pin's object".to_owned());
        }
        return Ok(out);
    }
}

/// The JSON string `text` starts with, and the text after it.
fn pin_string(text: &str) -> Result<(String, &str), String> {
    let inner = text
        .strip_prefix('"')
        .ok_or_else(|| format!("`{}` is no string", text.chars().take(16).collect::<String>()))?;
    let end = inner.find(['"', '\\']).ok_or("a string has no closing quote")?;
    if inner.as_bytes()[end] == b'\\' {
        return Err(format!("\"{}\" holds an escape", &inner[..end]));
    }
    Ok((inner[..end].to_owned(), &inner[end + 1..]))
}

/// The lowercase hex of `id`.
pub(crate) fn pin_hex(id: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in id {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}

fn pin_unhex(hex: &str) -> Option<[u8; 32]> {
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        _ => None,
    };
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut id = [0; 32];
    for (at, byte) in id.iter_mut().enumerate() {
        *byte = digit(bytes[2 * at])? << 4 | digit(bytes[2 * at + 1])?;
    }
    Some(id)
}

/// The pin's text for `libraries`, as the build script writes it beside the libraries it made
/// and as the tree keeps it.
pub(crate) fn pin_text(libraries: &[(&str, [u8; 32])]) -> String {
    let mut out = String::from("{\n");
    for (at, (name, id)) in libraries.iter().enumerate() {
        out.push_str("  \"");
        out.push_str(name);
        out.push_str("\": \"");
        out.push_str(&pin_hex(id));
        out.push('"');
        if at + 1 < libraries.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}
