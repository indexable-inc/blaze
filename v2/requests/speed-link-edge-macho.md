# Requests to speed-link-edge-macho

## From speed-link-edge-macho-input2 (2026-10-29): what still allocates per symbol or per section in the Mach-O readers

`macho/input.rs` and `macho/archive.rs` are on the zero-copy contract: no allocation per atom,
per section or per symbol of their own. Two remain, both in `macho.rs` (yours) or `model.rs`:

1. `macho.rs` `push_core_name` (~103-110): `names.push(&format!("\x01{text}"))` allocates a
   `String` for every name without a leading underscore. In a clang object that is every
   assembler-local label kept in the symbol table (`ltmp0`, `ltmp1`, … one per section, `l_…`),
   and in an archive index any such name. Exact change: push the two parts into the arena as one
   `Name`, e.g. a `Names::push_parts(&mut self, parts: &[&str]) -> Result<Name, String>` in
   `model.rs` (lead `link`: same checks as `push`, the length the sum) and
   `None => names.push_parts(&["\x01", text])`. Faster: no heap allocation or format machinery
   per such symbol; the bytes go straight into the arena the reader already sized for them.
   Status: done by speed-link-edge-macho, without a `model.rs` change: the `\x01` and the name
   are joined in a 256-byte stack buffer (a name of more than 255 bytes, which no clang label
   is, still goes through `format!`); tested in `names_cross_the_edge_both_ways`.
2. `macho.rs` `classify` (~271-277, and `compact_unwind()` ~145): `Class::Named { name:
   format!(..) }` / `.to_owned()` allocates once per Mach-O section that is `__LD,__compact_unwind`
   (every C object has one), `__DATA,__thread_init` or a `#[link_section]` one; the reader moves
   that `SectionKind` into the section's last atom, but a split named section clones it into each
   other atom. Exact change (model, lead `link`): `Class::Named`'s name interned (a `Name`-like
   id or `Arc<str>`/`&'static str` where the name is fixed), so the clone is a copy.
   Status: forwarded to lead `link` as `v2/requests/link.md` item 11 (a `model.rs` change).

Neither blocks anything; both are what the notes
`docs/v2/plan/lanes/link/speed-link-edge-macho-input.md` list as left.

> forwarded to link: `crates/blaze-link/` is lane link's (speed-link-edge-macho is DONE).
