//! The outline lane's oracle: every `.rs` file under the directories given is outlined by the
//! definition ([`reference`]: every body parsed with the rest, `Bodies::Parsed`) and by the
//! path under test ([`candidate`]: `outline`, bodies left unparsed where the grammar can), and
//! the two are compared: every row with its keys and digests, every attribute, the flags and
//! what is derived from them equal, and each finding of the candidate one of the definition's.
//! A candidate may refuse a text by type; a refusal is counted by its kind and is never a
//! difference. Three more lines say what no comparison of two paths can:
//!
//! - "every key of the candidate, folded": one digest over every key the candidate made, in
//!   order. A change that must move no key prints the same fold before and after (the path the
//!   candidate replaced, a walk of the parser's events over a text with its `fn` bodies cut
//!   out, was deleted 2026-11-16 at folds 3e0382dd16a6 for library/ and, for tempo's packages,
//!   f516be093fef read as 2024 and 7b750af10a10 in each package's own edition).
//! - "tokens handed over": each file written again from its tokens (tests/render) and outlined
//!   from the tokens (`outline_tokens`) and from the text written: equal, or the run fails.
//!
//! The run ends with one line of counts and exits 1 when any file differs.
//!
//! To put a new path under the oracle, change the body of [`candidate`] and [`Refused`]: the
//! rest stays.
//!
//! Arguments: directories, and `@file` for a file of directories, one a line. A file that is
//! not UTF-8 is no source file: left out and counted.

#![feature(allocator_api)]

use std::{alloc::Global, collections::BTreeMap, path::PathBuf, time::Instant};

use blaze_file_items::{Bodies, Outline, TokenColumns, chain_at, outline, outline_tokens, outline_with};
use blaze_surface::{Edition, ErrorOrigin, ItemIndex};

#[path = "../tests/render/mod.rs"]
mod render;

/// The tokens proof's counts: files whose outline from the tokens handed over is the outline
/// of the text written, files where it is not, files refused, and files whose rendering the
/// lexer reads as other tokens than those written (the proof's own input is wrong there).
#[derive(Default)]
struct TokensProof {
    equal: usize,
    different: usize,
    refused: usize,
    unfaithful: usize,
}

/// One file of the tokens proof: `text` written again from its tokens (tests/render: one line
/// feed for every stretch between two tokens, comments gone), then outlined from the tokens
/// handed over (`outline_tokens`) and from the text written (`outline`). Equal is every
/// row, key, digest, attribute and flag, what is derived from them, and the parser's findings;
/// the lexer's findings are the text path's alone.
fn tokens_proof(
    file: &std::path::Path,
    text: &str,
    edition: Edition,
    again: &mut String,
    columns: &mut TokenColumns<'_>,
    written: &mut Vec<(u32, u32, u32)>,
    proof: &mut TokensProof,
    heap: &'static blaze_own::Heap,
) {
    if let Err(why) = render::rerender(text, edition, again, columns, written) {
        proof.refused += 1;
        println!("TOKENS REFUSED {}: {why}", file.display());
        return;
    }
    if !render::lexes_back(again, edition, written) {
        proof.unfaithful += 1;
        println!("TOKENS: the text written lexes to other tokens: {}", file.display());
        return;
    }
    let from_tokens = match outline_tokens(again, columns, edition, heap.alloc()) {
        Ok(outline) => outline,
        Err(why) => {
            proof.refused += 1;
            println!("TOKENS REFUSED {}: {why}", file.display());
            return;
        }
    };
    let from_text = outline(again, edition, heap.alloc());
    let parser_said = |of: &Outline<'_>| {
        let all: Vec<(u32, u32, String)> = of
            .errors()
            .iter()
            .filter(|error| error.origin == ErrorOrigin::Parser)
            .map(|error| (error.range.start, error.range.end, of.message(error).to_owned()))
            .collect();
        all
    };
    let lexed_none = from_tokens.errors().iter().all(|error| error.origin == ErrorOrigin::Parser);
    if rows_equal(&from_text, &from_tokens, again) && parser_said(&from_text) == parser_said(&from_tokens) && lexed_none {
        proof.equal += 1;
    } else {
        proof.different += 1;
        if proof.different <= 8 {
            let at = from_text.items().iter().zip(from_tokens.items()).position(|(a, b)| a != b);
            println!(
                "TOKENS DIFFERENT {}: {} entries from the text against {} from the tokens, {} parser findings against {}; first differing entry {at:?}",
                file.display(),
                from_text.items().len(),
                from_tokens.items().len(),
                parser_said(&from_text).len(),
                parser_said(&from_tokens).len(),
            );
        }
    }
}

/// Why the candidate did not outline a text: one variant a kind the run counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Refused {
    /// A text longer than `u32::MAX` bytes: an outline's ranges are `u32` pairs.
    TooLong,
}

/// The definition: the outline with every body parsed with the rest.
fn reference(text: &str, edition: Edition, heap: &'static blaze_own::Heap) -> Outline<'static> {
    outline_with(text, edition, Bodies::Parsed, heap.alloc())
}

/// The path under test.
fn candidate(text: &str, edition: Edition, heap: &'static blaze_own::Heap) -> Result<Outline<'static>, Refused> {
    if u32::try_from(text.len()).is_err() {
        return Err(Refused::TooLong);
    }
    Ok(outline(text, edition, heap.alloc()))
}

/// The edition of the package a file is in, for `--own-edition`: the `edition` of the nearest
/// `Cargo.toml` above it that has a `[package]`, 2015 when it writes none, and the
/// `[workspace.package]` edition of a manifest further up when it says `workspace = true`.
/// Read by line, which is enough for the manifests cargo's registry and tempo hold; a manifest
/// this cannot read ends the run by name.
fn own_edition(file: &std::path::Path, known: &mut BTreeMap<PathBuf, Edition>) -> Edition {
    let dir = file.parent().expect("a file is in a directory").to_path_buf();
    if let Some(&edition) = known.get(&dir) {
        return edition;
    }
    let named = |value: &str| match value.trim().trim_matches('"') {
        "2015" => Some(Edition::E2015),
        "2018" => Some(Edition::E2018),
        "2021" => Some(Edition::E2021),
        "2024" => Some(Edition::E2024),
        _ => None,
    };
    // What `section` of a manifest writes for `edition`: its value, or "workspace".
    let written = |manifest: &str, section: &str| {
        let lines = manifest.lines().map(str::trim).skip_while(|line| *line != section).skip(1);
        lines.take_while(|line| !line.starts_with('[')).find_map(|line| {
            let rest = line.strip_prefix("edition")?.trim_start();
            match rest.strip_prefix('=') {
                Some(value) if value.contains("workspace") => Some("workspace".to_owned()),
                Some(value) => Some(value.trim().to_owned()),
                None => rest.starts_with(".workspace").then(|| "workspace".to_owned()),
            }
        })
    };
    let mut inherits = false;
    let mut found = None;
    for up in dir.ancestors() {
        let Ok(manifest) = std::fs::read_to_string(up.join("Cargo.toml")) else { continue };
        if !inherits {
            if !manifest.lines().any(|line| line.trim() == "[package]") {
                continue;
            }
            match written(&manifest, "[package]").as_deref() {
                None => found = Some(Edition::E2015),
                // The same manifest may be the workspace's root: read on below.
                Some("workspace") => inherits = true,
                Some(value) => match named(value) {
                    Some(edition) => found = Some(edition),
                    None => {
                        eprintln!("{}: an edition that is none: {value}", up.join("Cargo.toml").display());
                        std::process::exit(2);
                    }
                },
            }
        }
        if inherits {
            found = written(&manifest, "[workspace.package]").as_deref().and_then(named);
        }
        if found.is_some() {
            break;
        }
    }
    let Some(edition) = found else {
        eprintln!("{}: no manifest above it names its package's edition", file.display());
        std::process::exit(2);
    };
    known.insert(dir, edition);
    edition
}

/// Every finding of an outline by where it is, whose it is and what it says, sorted.
fn said(of: &Outline<'_>) -> Vec<(u32, u32, u8, String)> {
    let mut all: Vec<(u32, u32, u8, String)> =
        of.errors().iter().map(|error| (error.range.start, error.range.end, error.origin as u8, of.message(error).to_owned())).collect();
    all.sort();
    all
}

/// Whether each finding of `few` is one of `many`, counted: two of a kind need two.
fn within(few: &[(u32, u32, u8, String)], many: &[(u32, u32, u8, String)]) -> bool {
    few.iter().all(|one| few.iter().filter(|it| *it == one).count() <= many.iter().filter(|it| *it == one).count())
}

/// Whether two outlines of `text` are equal in all but their findings: every row with its keys
/// and digests, every attribute, the flags, and what is derived from them.
fn rows_equal(old: &Outline<'_>, new: &Outline<'_>, text: &str) -> bool {
    old.items() == new.items()
        && old.file_attrs() == new.file_attrs()
        && old.items().iter().zip(new.items()).all(|(a, b)| old.attrs(a) == new.attrs(b))
        && (old.flags(), old.own_flags()) == (new.flags(), new.own_flags())
        && derived_equal(old, new, text)
}

/// What other crates read of an outline beyond its rows, compared by value so that the oracle
/// does not rest on `Outline`'s `==` covering their inputs: the file's surface
/// (`Outline::surface`), and for each entry its names digest (crate::names_digest) and its
/// chain (crate::chain: kind, name, disambiguator and head up the parents).
fn derived_equal(old: &Outline<'_>, new: &Outline<'_>, text: &str) -> bool {
    old.surface() == new.surface()
        && old.items().len() == new.items().len()
        && (0..old.items().len()).all(|at| {
            let at = ItemIndex(u32::try_from(at).expect("an outline counts in u32"));
            old.names_digest(at) == new.names_digest(at) && chain_at(old, at, text) == chain_at(new, at, text)
        })
}

fn main() {
    let heap = blaze_own::Heap::process();
    let mut files: Vec<PathBuf, Global> = Vec::new_in(Global);
    let mut stack: Vec<PathBuf, Global> = Vec::new_in(Global);
    let mut own = false;
    for arg in std::env::args().skip(1) {
        if arg == "--own-edition" {
            own = true;
            continue;
        }
        match arg.strip_prefix('@') {
            Some(list) => {
                let Ok(lines) = std::fs::read_to_string(list) else {
                    eprintln!("not a file that reads: {list}");
                    std::process::exit(2);
                };
                stack.extend(lines.lines().filter(|it| !it.is_empty()).map(PathBuf::from));
            }
            None => stack.push(PathBuf::from(arg)),
        }
    }
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("not a directory that reads: {}", dir.display());
            std::process::exit(2);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    let (mut read, mut not_utf8, mut bytes, mut entries, mut findings) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut keys = blaze_ids::Hasher::new("blaze-file-items oracle: every key of the candidate");
    let (mut equal, mut different, mut refused_bytes) = (0usize, 0usize, 0usize);
    let mut editions: BTreeMap<PathBuf, Edition> = BTreeMap::new();
    let mut by_edition = [0usize; 4];
    let mut refused: BTreeMap<Refused, usize> = BTreeMap::new();
    let (mut reference_time, mut candidate_time) = (0f64, 0f64);
    let mut proof = TokensProof::default();
    let (mut again, mut written) = (String::new(), Vec::new());
    let mut columns = TokenColumns::new_in(heap.alloc());
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            not_utf8 += 1;
            continue;
        };
        read += 1;
        bytes += text.len();
        let edition = match own {
            true => own_edition(file, &mut editions),
            false => Edition::E2024,
        };
        by_edition[edition as usize] += 1;
        let start = Instant::now();
        let old = reference(&text, edition, heap);
        reference_time += start.elapsed().as_secs_f64();
        entries += old.items().len();
        findings += old.errors().len();
        let start = Instant::now();
        let new = candidate(&text, edition, heap);
        candidate_time += start.elapsed().as_secs_f64();
        if new.is_ok() {
            tokens_proof(file, &text, edition, &mut again, &mut columns, &mut written, &mut proof, heap);
        }
        // Every key the candidate made, folded in order into one digest the run prints: two
        // runs over the same files print the same fold exactly when every entry's digest,
        // signature digest, disambiguator, names digest and chain, and every file's surface,
        // are the same bytes. What a change that must move no key is checked by.
        if let Ok(new) = &new {
            keys.id(&new.surface());
            for (at, item) in new.items().iter().enumerate() {
                let at = ItemIndex(u32::try_from(at).expect("an outline counts in u32"));
                keys.id(&item.hash).id(&item.signature_hash).update(&item.disambiguator.to_le_bytes());
                keys.id(&new.names_digest(at)).id(&chain_at(new, at, &text));
            }
        }
        match new {
            Err(why) => {
                *refused.entry(why).or_default() += 1;
                refused_bytes += text.len();
            }
            // Equal: every row, attribute, flag and digest, and what is derived from them; and
            // each finding of the candidate is one of the definition's (a body left unparsed
            // gives up the parser's findings inside it and nothing else: the lexer's are all
            // there, the whole text being lexed either way).
            Ok(new) if rows_equal(&old, &new, &text) && within(&said(&new), &said(&old)) => equal += 1,
            Ok(new) => {
                different += 1;
                if different <= 8 {
                    // The first row that differs, both ways, so a difference names its item.
                    let at = old.items().iter().zip(new.items()).position(|(a, b)| a != b);
                    println!(
                        "DIFFERENT {}: {} entries against {}, {} findings against {}; first differing entry {at:?}",
                        file.display(),
                        old.items().len(),
                        new.items().len(),
                        old.errors().len(),
                        new.errors().len(),
                    );
                    if let Some(at) = at {
                        println!("  reference {:?}", old.items()[at]);
                        println!("  candidate {:?}", new.items()[at]);
                        // The last entry that differs is the innermost: a parent's digest
                        // moves with its child's.
                        let last = old.items().iter().zip(new.items()).rposition(|(a, b)| a != b).expect("one differs");
                        println!("  last differing entry {last}:");
                        println!("  reference {:?}", old.items()[last]);
                        println!("  candidate {:?}", new.items()[last]);
                        println!("  its text: {:?}", &text[old.items()[last].range.to_usize()].chars().take(400).collect::<String>());
                    }
                    let finding = |of: &Outline<'_>, at: usize| {
                        of.errors().get(at).map(|error| format!("{:?} {:?}", error.range, of.message(error)))
                    };
                    let first = (0..old.errors().len().max(new.errors().len())).find(|&at| finding(&old, at) != finding(&new, at));
                    if let Some(at) = first {
                        println!("  first differing finding {at}: reference {:?}, candidate {:?}", finding(&old, at), finding(&new, at));
                    }
                }
            }
        }
    }
    for (why, count) in &refused {
        println!("refused {why:?}: {count} files");
    }
    println!("every key of the candidate, folded: {:?}", keys.finish());
    println!(
        "editions ({}): {} files read as 2015, {} as 2018, {} as 2021, {} as 2024",
        if own { "each package's own, from its manifest" } else { "every file as 2024" },
        by_edition[0],
        by_edition[1],
        by_edition[2],
        by_edition[3]
    );
    println!(
        "oracle: {read} files, {:.1} MB, {entries} entries, {findings} findings ({not_utf8} files not UTF-8 left out); {equal} equal, {different} different, {} refused ({:.1} MB); reference {:.1} ms, candidate {:.1} ms on one thread",
        bytes as f64 / 1e6,
        refused.values().sum::<usize>(),
        refused_bytes as f64 / 1e6,
        reference_time * 1e3,
        candidate_time * 1e3,
    );
    println!(
        "tokens handed over (each file written again from its tokens, one line feed for every stretch between two; `outline_tokens` against the outline of the text written): {} equal, {} different, {} refused, {} whose rendering lexes to other tokens",
        proof.equal, proof.different, proof.refused, proof.unfaithful
    );
    if different != 0 || proof.different != 0 || proof.refused != 0 || proof.unfaithful != 0 {
        std::process::exit(1);
    }
}
