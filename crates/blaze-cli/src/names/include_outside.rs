//! An include that left its own package and stayed inside the workspace (decision D111,
//! 2026-11-16; the owner: "sure allow it but give a warning for include str"): allowed, and
//! filed as a finding of the crate's names record at the call, placed item-relative (rule 71)
//! like an unresolved path ([`super::UNRESOLVED`]'s message form: file, item chain, edition,
//! words), with no level in the record (rule 81): the printer draws it as a warning after the
//! answer, and it never fails a command (main.rs `every_rule`). A repeat run shows it from the
//! record, as every finding of a standing record.
//!
//! The words name the macro as written at the call in its shape, the file read and the
//! package it lies outside of, each workspace-relative (the texts' `/0/` cut): "`include_str!`
//! reads `other-dir/file.txt`, outside its package `crates/a`"; a member's proc macro's read
//! (blaze-scope resolve.rs `Host::run_proc_macro`, from its read list, so a kept answer gives
//! the same finding): "`#[derive(Abi)]` reads `crates/sibling/y.json`, outside its package
//! `crates/m`".

use std::alloc::Global;

use blaze_scope::resolve::CrateNames;

use super::Unresolved;

/// The rule of an include that reads outside its own package, inside the workspace (D111).
/// A warning: blaze-report's words, number 21.
pub(crate) const INCLUDE_OUTSIDE_PACKAGE: u16 = 21;

/// One read the texts were told of (job_texts.rs `left_package`: an include's, or a member's
/// proc macro's, D111): the calling file (as the texts name it, `/0/..`), the byte where the
/// call's path starts, the call's shape, the macro's path as written at the call (never
/// resolved: two spellings give two messages), and the file read (`/0/..`).
pub(crate) type Left<'n> = (&'n str, u32, blaze_surface::CallShape, &'n str, &'n str);

/// The call as the message writes it: `name!`, `#[name]`, `#[derive(name)]`.
fn write_call(out: &mut blaze_own::Vec<u8, Global>, shape: blaze_surface::CallShape, name: &str) {
    let (before, after): (&[u8], &[u8]) = match shape {
        blaze_surface::CallShape::FnLike => (b"", b"!"),
        blaze_surface::CallShape::Attr => (b"#[", b"]"),
        blaze_surface::CallShape::Derive => (b"#[derive(", b")]"),
    };
    out.extend_from_slice(before);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(after);
}

/// The findings of `left` for the crate `c` whose package folder is `package` (as the texts
/// name it), sorted by (message, range) and without repeats (the host may be asked twice for
/// one call). A call whose file or item is not found is placed at the crate (an empty file
/// piece), never dropped.
pub(super) fn findings(c: &CrateNames<'_>, left: &[Left<'_>], package: &str) -> blaze_own::Vec<Unresolved, Global> {
    let mut own = blaze_own::Vec::new_in(Global);
    let cut = |path: &str| -> std::string::String { std::string::String::from(path.strip_prefix("/0/").unwrap_or(path)) };
    for &(from, at, shape, call, path) in left {
        let mut message = blaze_own::Vec::new_in(Global);
        let mut range = (0u32, 0u32);
        let place = c.files.iter().find(|file| file.path == from).and_then(|file| Some((*file, crate::work::item_holding(&file.outline, at)?)));
        match place {
            Some((file, item)) => {
                let start = at.saturating_sub(item.range.start);
                range = (start, start + call.len() as u32);
                message.extend_from_slice(cut(file.path).as_bytes());
                message.push(0x1F);
                let chain = crate::work::item_chain(&file.outline, item, file.text);
                for shift in (0..16).rev() {
                    message.push(b"0123456789abcdef"[((chain >> (shift * 4)) & 0xF) as usize]);
                }
            }
            None => message.push(0x1F),
        }
        message.push(0x1F);
        message.push(b'0' + c.edition.byte());
        message.push(0x1F);
        write_call(&mut message, shape, call);
        message.push(0x1F);
        message.extend_from_slice(cut(path).as_bytes());
        message.push(0x1F);
        message.extend_from_slice(cut(package).as_bytes());
        own.push(Unresolved { at: range, message });
    }
    own.sort_unstable_by(|a, b| (&a.message[..], a.at).cmp(&(&b.message[..], b.at)));
    let mut kept: blaze_own::Vec<Unresolved, Global> = blaze_own::Vec::new_in(Global);
    for finding in own {
        if kept.last().is_some_and(|last| last.message == finding.message && last.at == finding.at) {
            continue;
        }
        kept.push(finding);
    }
    kept
}
