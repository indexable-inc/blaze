//! Where a local or a label a body-local `macro_rules!` template names resolves: at the
//! definition.
//!
//! The Reference, "Macros By Example", Hygiene: "Macros by example have mixed-site hygiene.
//! This means that loop labels, block labels, and local variables are looked up at the macro
//! definition site while other symbols are looked up at the macro invocation site." So in
//!
//! ```text
//! fn decode(log: &Log, block: u64) -> Result<..> {
//!     macro_rules! ignored { ($e:ty, $n:literal) => {{ decode_event::<$e>(log, $n, block)?; .. }} }
//!     .. ignored!(A, "A") ..
//! }
//! ```
//!
//! (tempo's `zone_checker::l1::events::decode_portal_event`) the template's `log` and `block`
//! are the parameters: they are in scope where `ignored!` is written. And in
//! `macro_rules! m { () => { y } } let y = 1; m!()` the template's `y` is no local: no `y` is
//! in scope at the definition, and the caller's `let y` written after it is not seen.
//!
//! How the lowering does it. A name is bound and looked up by its key
//! ([`Machine::local_key`]: its hash and its local hygiene, blaze-expand's `body` module). An
//! expansion's own tokens carry a nonzero local hygiene, so a template's `log` is keyed apart
//! from the parameter's `log` (hygiene 0): its own `let`s see it, nothing of the caller's does.
//! That half is right; the other half is the definition site. Each local hygiene is noted, at
//! the expansion whose words first carry it, with the definition that expansion came from
//! ([`TemplateSite`]): for a `macro_rules!` the body wrote, how many locals were bound and how
//! many loops and labelled blocks were open when it was written, and the hygiene of the text
//! that wrote it; for any other macro, none (a macro defined outside the body sees none of
//! its locals). A lookup of a name of hygiene `h` first asks its own key (the expansion's own
//! bindings and labels); failing that, it asks again with the definition's hygiene, among the
//! bindings and labels that existed at the definition only. That strips one expansion at a
//! time, as many times as definitions nest (a work list, bounded by the sites noted). A name
//! passed in as `$x` keeps the hygiene of the text that wrote it (0 for the caller's own), so
//! it resolves where the caller wrote it, unchanged.
//!
//! Why "the locals bound at the definition" is a count: `locals` is a stack, a `macro_rules!`
//! is in textual scope only until its block closes ([`Machine::local_macros`]), and nothing
//! bound before it is popped while its block is open, so the bindings it sees are exactly
//! the stack's first `locals` entries; a later binding of the same name sits above them and
//! is skipped by its `prev` link. The same holds for `entries` and labels.

use super::*;

/// One local hygiene's definition: what a template of that hygiene sees.
#[derive(Clone, Copy, Debug)]
pub(super) struct TemplateSite {
    /// The local hygiene of an expansion's words (never 0).
    pub(super) hygiene: u64,
    /// The local hygiene of the text that wrote the `macro_rules!`: 0 when the item's own
    /// text did, else that expansion's.
    pub(super) outer: u64,
    /// How many of the machine's `locals` were bound at the definition: the bindings a
    /// template sees. 0 for a macro the body did not define.
    pub(super) locals: u32,
    /// How many of the machine's `entries` (loops, labelled blocks) were open at the
    /// definition: the labels a template sees. 0 for a macro the body did not define.
    pub(super) entries: u32,
}

/// The key a name of hash `hash` and local hygiene `hygiene` is bound and looked up by.
pub(super) fn keyed(hash: u64, hygiene: u64) -> u64 {
    match hygiene {
        0 => hash,
        hygiene => hash ^ hygiene.rotate_left(17),
    }
}

/// The definition of a body's macro read from the machine's text: a `macro_rules!`'s rules
/// (the text inside its delimiters), a `macro`'s trees after its name; `None` when its text
/// has no such tree. One home for the reading [`Machine::local_macros`] hands the expander
/// and [`Machine::note_templates`] matches.
pub(super) fn form_of<'s>(text: &'s str, l: &LocalMacroAt) -> Option<crate::driver::LocalForm<'s>> {
    use crate::driver::LocalForm;
    let after = text.get(l.name.end as usize..l.item.end as usize)?.trim();
    if l.macro2 {
        return (!after.is_empty()).then_some(LocalForm::Macro2(after));
    }
    let tree = after.strip_suffix(';').map_or(after, str::trim_end);
    tree.get(1..tree.len().checked_sub(1)?).map(LocalForm::Rules)
}

impl Machine<'_, '_, '_> {
    /// The `macro_rules!` of the body the call `request` expanded with, when it was one: the
    /// choice by_name.rs `ByName::local` makes over [`Machine::local_macros`] (a one-name
    /// path, the last written of the name in textual scope).
    fn local_macro_of(&self, request: &Request) -> Option<LocalMacroAt> {
        let text = self.text();
        let path = request.path_range.of(text).trim();
        self.local_macros.iter().rev().find(|l| l.name.of(text) == path && form_of(text, l).is_some()).copied()
    }

    /// Notes the definition site of each local hygiene `words` (an expansion of `request`,
    /// just appended) carry first: see the module's doc.
    pub(super) fn note_templates(&mut self, request: &Request, words: &[blaze_expand::BodyWord]) {
        let (outer, locals, entries) = match self.local_macro_of(request) {
            Some(l) => (self.hygiene_at(l.item.start), l.locals, l.entries),
            None => (0, 0, 0),
        };
        let mut last = 0u64;
        for w in words {
            if w.hygiene == 0 || w.hygiene == last {
                continue;
            }
            last = w.hygiene;
            if self.template_site(w.hygiene).is_some() {
                continue;
            }
            self.template_sites.push(TemplateSite { hygiene: w.hygiene, outer, locals, entries });
        }
    }

    fn template_site(&self, hygiene: u64) -> Option<TemplateSite> {
        self.template_sites.iter().find(|s| s.hygiene == hygiene).copied()
    }

    /// The name `name`'s hash and its local hygiene.
    fn hash_and_hygiene(&self, name: NameIdx) -> (u64, u64) {
        (self.out.names[name.0 as usize], self.hygiene_at(self.out.name_ranges[name.0 as usize].start))
    }

    /// The innermost binding of key `key` among the first `below` of `locals`, inside the
    /// barrier.
    fn bound_below(&self, key: u64, below: usize) -> Option<PatId> {
        let mut at = *self.top.get(&key)?;
        loop {
            if at == NONE {
                return None;
            }
            if (at as usize) < below {
                break;
            }
            at = self.locals[at as usize].prev;
        }
        (at >= self.barrier).then(|| PatId(self.locals[at as usize].pat))
    }

    /// The binding the local name `name` resolves to: its own hygiene's, else the one in
    /// scope at the definition that wrote it (the module's doc).
    pub(crate) fn lookup_name(&self, name: NameIdx) -> Option<PatId> {
        let (hash, mut hygiene) = self.hash_and_hygiene(name);
        let mut below = self.locals.len();
        // Each step goes to an earlier definition's hygiene: at most one per site noted.
        for _ in 0..=self.template_sites.len() {
            if let Some(pat) = self.bound_below(keyed(hash, hygiene), below) {
                return Some(pat);
            }
            if hygiene == 0 {
                return None;
            }
            let site = self.template_site(hygiene)?;
            if site.locals == 0 {
                return None;
            }
            hygiene = site.outer;
            below = below.min(site.locals as usize);
        }
        None
    }

    /// The entry the label `name` of a `break` or `continue` leaves: its own hygiene's, else
    /// one open at the definition that wrote it (the module's doc).
    pub(super) fn find_label(&self, name: NameIdx, want_loop: bool) -> Result<usize, DiagKind> {
        let (hash, mut hygiene) = self.hash_and_hygiene(name);
        let mut below = self.entries.len();
        for _ in 0..=self.template_sites.len() {
            match self.find_entry(keyed(hash, hygiene), want_loop, below) {
                Err(DiagKind::UndeclaredLabel) => {}
                found => return found,
            }
            if hygiene == 0 {
                break;
            }
            let Some(site) = self.template_site(hygiene) else { break };
            if site.entries == 0 {
                break;
            }
            hygiene = site.outer;
            below = below.min(site.entries as usize);
        }
        Err(DiagKind::UndeclaredLabel)
    }
}

#[cfg(test)]
mod tests;
