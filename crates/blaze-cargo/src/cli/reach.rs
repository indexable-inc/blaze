//! `blaze reach [ROOT] [--json]`: what the checkout's edits since `ROOT` reach
//! ([`Request::Reach`]): the items whose hashes changed with the facts of each that did, the
//! functions that would compile again and the tests that reach changed code, building only the
//! proc macros and build scripts' runs the front end applies. Each answer ends with the root the
//! checkout is at now, to pass to the next; without `ROOT` only that root is said. Both views are
//! checked as `blaze check` in this directory checks them.

use std::{ffi::OsString, fmt::Write as _};

use blaze_api::{Reach, Request, ResponseRef, ViewRef};
use blaze_core::exit::Exit;
use blaze_ids::ContentId;
use blaze_vfs::View;

use super::{
    args::{self, Asked},
    daemon::Start,
    observe, say,
};

const REACH_USAGE: &str = "blaze reach [ROOT] [--json]: what the edits since ROOT (a root an earlier `blaze reach` said) reach";

/// `blaze reach`: its exit code.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    match reach(words, color) {
        Ok(()) => 0,
        Err((what, code)) => {
            eprint!("{}", say::error(&what, color));
            code
        }
    }
}

fn reach(words: Vec<OsString>, color: bool) -> Result<(), (String, i32)> {
    let usage = |what: String| (format!("{REACH_USAGE}: {what}"), Exit::Usage.code());
    let failed = |what: String| (what, Exit::Failed.code());
    let (mut from, mut json) = (None, false);
    for word in words {
        let Some(word) = word.to_str() else {
            return Err(usage(format!("{} is not UTF-8", word.to_string_lossy())));
        };
        match word {
            "--json" => json = true,
            _ if word.starts_with('-') => return Err(usage(format!("not `{word}`"))),
            _ if from.is_some() => return Err(usage(format!("one root, not `{word}` too"))),
            _ => {
                from = Some(
                    root_of(word)
                        .ok_or_else(|| usage(format!("`{word}` is no root: 64 hex digits")))?,
                )
            }
        }
    }
    // The targets `blaze check` here sends: the same config files, environment and features.
    let asked = match args::parse(vec![OsString::from("check")]) {
        Ok(Asked::Cargo(asked)) => asked,
        _ => return Err(failed("`blaze reach` could not read itself as `blaze check`".to_owned())),
    };
    let (_, cargo) = super::prepared_cargo(&asked, color).map_err(failed)?;
    let head = cargo.root.clone(); // clone: the head's name, beside the request that owns it
    let Request::Check { targets, .. } = super::request(&asked, cargo).map_err(failed)? else {
        return Err(failed("`blaze reach` made no check's targets of `blaze check`".to_owned()));
    };
    crate::toolchain::ensure().map_err(failed)?;
    let mut now = None;
    observe::ask(Request::Open { view: ViewRef::Head(head) }, Start::IfMissing, |response| {
        if let ResponseRef::Opened { root } = response {
            now = Some(root);
        }
        Ok(())
    })
    .map_err(failed)?;
    let now = now
        .ok_or_else(|| failed("the daemon answered the checkout's open with no root".to_owned()))?;
    let reached = match from {
        None => None,
        Some(from) => {
            let (from, to) =
                (ViewRef::Root(View { root: from }), ViewRef::Root(View { root: now }));
            let mut reached = None;
            observe::ask(Request::Reach { to, from, targets }, Start::Never, |response| {
                if let ResponseRef::Reached(it) = response {
                    reached = Some(it);
                }
                Ok(())
            })
            .map_err(failed)?;
            Some(reached.ok_or_else(|| {
                failed("the daemon answered `blaze reach` with no answer".to_owned())
            })?)
        }
    };
    let text = match json {
        true => {
            let value = serde_json::json!({ "root": format!("{now:x}"), "reached": reached });
            format!("{value}\n")
        }
        false => lines(reached.as_ref(), now),
    };
    print!("{text}");
    Ok(())
}

/// `reached` as lines, one an item (its path, then each fact of it that changed), a function
/// compiled again and a test reached, and the root the checkout is at last.
fn lines(reached: Option<&Reach>, now: ContentId) -> String {
    let mut text = String::new(); // alloc: CLI edge, the answer's text, once
    for item in reached.iter().flat_map(|it| &it.items) {
        let facts: Vec<&str> = item.facts.iter().map(|it| it.what.as_str()).collect(); // alloc: CLI edge, a line's facts
        let _ = writeln!(text, "item {}: {}", item.path, facts.join("; "));
    }
    for function in reached.iter().flat_map(|it| &it.compiled) {
        let _ = writeln!(text, "compiled {function}");
    }
    for test in reached.iter().flat_map(|it| &it.tests) {
        let _ = writeln!(text, "test {test}");
    }
    let _ = writeln!(text, "root {now:x}");
    text
}

/// The root `word` names: 64 hex digits.
fn root_of(word: &str) -> Option<ContentId> {
    let bytes = word.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let digit = |it: u8| char::from(it).to_digit(16).map(|it| it as u8);
    let mut id = [0u8; 32];
    for (at, pair) in bytes.chunks_exact(2).enumerate() {
        id[at] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(ContentId(id))
}
