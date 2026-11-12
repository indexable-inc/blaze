//! Words and numbers as the screen writes them: a time, a width, a path cut in the middle.

use alloc::{
    format,
    string::{String, ToString as _},
    vec::Vec,
};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// The columns `text` takes.
pub fn width(text: &str) -> usize {
    text.width()
}

/// A time: `12ms`, `0.70s`, `19.4s`, `4m30s`. Under a second it is whole milliseconds, so an
/// edit's line says the number that matters; from a minute on, whole seconds.
pub fn time(ms: u64) -> String {
    match ms {
        0..100 => format!("{ms}ms"),               // alloc: a number shown
        100..10_000 => format!("{}.{:02}s", ms / 1000, ms % 1000 / 10), // alloc: as above
        10_000..60_000 => format!("{}.{}s", ms / 1000, ms % 1000 / 100), // alloc: as above
        _ => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000), // alloc: as above
    }
}

/// A live timer: tenths of a second, so it changes no more often than the region is redrawn.
pub fn timer(ms: u64) -> String {
    match ms {
        0..60_000 => format!("{}.{}s", ms / 1000, ms % 1000 / 100), // alloc: a number shown
        _ => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000), // alloc: as above
    }
}

/// Bytes in gigabytes: `20.1`.
pub fn gb(bytes: u64) -> String {
    format!("{}.{}", bytes / 1_000_000_000, bytes % 1_000_000_000 / 100_000_000) // alloc: a number shown
}

/// `text` in at most `most` columns: its end cut and marked `…` when it is longer.
pub fn fit(text: &str, most: usize) -> String {
    if width(text) <= most {
        return text.to_string(); // alloc: a cell of a row
    }
    let mut out = String::new(); // alloc: as above
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > most {
            break;
        }
        out.push(c);
        used += w;
    }
    if most > 0 {
        out.push('…');
    }
    out
}

/// `text` in at most `most` columns with its middle cut: `/root/home/…/debug/hello`. What a
/// path's reader needs is its start and its end.
pub fn middle(text: &str, most: usize) -> String {
    let total = width(text);
    if total <= most {
        return text.to_string(); // alloc: a cell of a row
    }
    if most < 5 {
        return fit(text, most);
    }
    let keep = most - 1;
    let tail = keep * 2 / 3;
    let head = keep - tail;
    let chars: Vec<char> = text.chars().collect(); // alloc: once a cut path
    let mut out = String::new(); // alloc: as above
    let mut used = 0;
    for &c in &chars {
        let w = c.width().unwrap_or(0);
        if used + w > head {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    let mut end = Vec::new(); // alloc: as above
    let mut used = 0;
    for &c in chars.iter().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > tail {
            break;
        }
        end.push(c);
        used += w;
    }
    out.extend(end.into_iter().rev());
    out
}

/// An item's path as a row names it, in at most `most` columns: what a macro's expansion
/// nested into it is said once (`<impl#0 in macro call 3ca1.0 in macro call 3ca1.0 ..>`
/// becomes `<impl#0 in macro>`), and a path still too long keeps its crate and its last
/// segments around a `…`: `libc::…::<impl#0 in macro>::default`.
pub fn item(path: &str, most: usize) -> String {
    const CALL: &str = " in macro call ";
    let mut flat = String::with_capacity(path.len()); // alloc: once an item shown
    let mut rest = path;
    let mut in_macro = false;
    while let Some(at) = rest.find(CALL) {
        flat.push_str(&rest[..at]);
        let after = &rest[at + CALL.len()..];
        let end = after.find([' ', '>', ':']).unwrap_or(after.len());
        rest = &after[end..];
        in_macro = true;
        if !rest.starts_with(CALL) {
            flat.push_str(" in macro");
            in_macro = false;
        }
    }
    if in_macro {
        flat.push_str(" in macro");
    }
    flat.push_str(rest);
    if width(&flat) <= most {
        return flat;
    }
    // Its segments, split at `::` outside angle brackets.
    let mut segments = Vec::new(); // alloc: once a cut item
    let (mut depth, mut from) = (0usize, 0usize);
    let bytes = flat.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'<' => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b':') => {
                segments.push(&flat[from..at]);
                at += 1;
                from = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    segments.push(&flat[from..]);
    if segments.len() < 3 {
        return middle(&flat, most);
    }
    let first = segments[0];
    let mut tail = String::new(); // alloc: as above
    let mut kept = 0;
    for segment in segments[1..].iter().rev() {
        let more = width(segment) + 2;
        if width(first) + 3 + width(&tail) + more > most && kept > 0 {
            break;
        }
        tail.insert_str(0, segment);
        tail.insert_str(0, "::");
        kept += 1;
    }
    let cut = match kept == segments.len() - 1 {
        true => format!("{first}{tail}"),   // alloc: as above
        false => format!("{first}::…{tail}"), // alloc: as above
    };
    middle(&cut, most)
}

/// A unit as a row names it, from the plane's name of it (`core 0.0.0 lib (host)`, `hello
/// 0.1.0 bin hello (host)`, `ring 0.17.8 build-script (host)`): the package, and its target
/// only where it is not the package's library. Two units of one package keep apart by that
/// target; what it is built for is said only when it is not the host.
pub fn unit(name: &str) -> String {
    let mut words = name.split(' ');
    let Some(package) = words.next() else { return String::new() };
    let rest: Vec<&str> = words.collect(); // alloc: once a row
    // `lowering /path`: a step of the plan, not a unit.
    if rest.first().is_some_and(|it| it.starts_with('/')) || rest.is_empty() {
        return name.to_string(); // alloc: as above
    }
    let mut out = package.to_string(); // alloc: as above
    let mut at = 0;
    // The version.
    if rest.get(at).is_some_and(|it| it.starts_with(|c: char| c.is_ascii_digit())) {
        at += 1;
    }
    let target: Vec<&str> =
        rest[at..].iter().copied().filter(|it| *it != "(host)" && *it != "lib").collect(); // alloc: as above
    match target.as_slice() {
        [] => {}
        ["bin", bin] if *bin == package => out.push_str(" (bin)"),
        ["build-script"] => out.push_str(" (build script)"),
        ["proc-macro"] | ["proc-macro", "(macro)"] => out.push_str(" (proc macro)"),
        more => {
            out.push_str(" (");
            out.push_str(&more.join(" ").replace(['(', ')'], "")); // alloc: as above
            out.push(')');
        }
    }
    out
}

/// `1 unit`, `3 units`.
pub fn count(n: u64, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),  // alloc: a count shown
        _ => format!("{n} {many}"), // alloc: as above
    }
}

/// `text` as a logfmt value: bare when it is one plain word, quoted otherwise.
pub fn value(text: &str) -> String {
    let bare = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'=' && b != b'\\');
    match bare {
        true => text.to_string(), // alloc: a field of a line
        false => {
            let mut out = String::with_capacity(text.len() + 2); // alloc: as above
            out.push('"');
            for c in text.chars() {
                match c {
                    '"' | '\\' => {
                        out.push('\\');
                        out.push(c);
                    }
                    '\n' | '\r' | '\x1b' => out.push(' '),
                    _ => out.push(c),
                }
            }
            out.push('"');
            out
        }
    }
}
