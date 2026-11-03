//! Mapping symbols: the `$a`, `$t` and `$d` that ARM ELF objects use to say
//! which bytes of a section are ARM code, Thumb code or data.
//!
//! Nothing runs differently for them, but a disassembler needs them to read a
//! literal pool as data and Thumb as Thumb, and a linker that swaps byte order
//! for BE8, or patches an erratum, needs them to find the instructions. So
//! they are part of what an object has to get right, and there is a
//! reference to get them right against: GNU as's ARM port, which is followed
//! here rule for rule, oddities included.
//!
//! GNU as keeps one state per section — nothing yet, data, ARM or Thumb — and
//! writes a symbol where the state changes:
//!
//! - an instruction switches to its instruction set, and raises the section's
//!   alignment to that set's (four bytes for ARM, two for Thumb) — but only
//!   when it changes the state, so ARM code after an alignment that already
//!   marked it ARM leaves the section's alignment alone;
//! - a data directive switches to data, except that data at the very start of
//!   a section marks nothing until code follows it, when `$d` goes back to
//!   the start;
//! - an alignment, a `.space` or a `.fill` marks its own start without that
//!   exception, as data or, for an alignment padded with no-ops, as code; and
//!   if the no-ops leave an odd few bytes over, those become data, with the
//!   code mark moved past them;
//! - a literal pool marks itself as data even if the state already is.
//!
//! Once the layout is known, a symbol with another after it at the same
//! address is dropped, and so is one at the very end of its section: that is
//! what GNU as's `check_mapping_symbols` leaves.
//!
//! They are an ELF convention — the ARM and AArch64 psABIs define them, and
//! only the ELF writer emits them — so a COFF or Mach-O object has none of
//! this, not even the alignment an instruction would raise its section to:
//! llvm-mc leaves `__text` at the alignment the source asked for. A flat
//! image keeps it, because what it is compared against is the ELF object
//! linked.

use crate::arch::{ArchState, Architecture};
use crate::assembler::Assembler;
use crate::section::SectionId;

/// A mapping symbol recorded while the source is read, placed once the layout
/// is known.
#[derive(Copy, Clone, Debug)]
pub struct MapEvent {
    /// The fragment the symbol is at the start of.
    pub frag: u32,
    pub name: &'static str,
    pub kind: MapEventKind,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MapEventKind {
    /// A symbol at the start of the fragment.
    At,
    /// The fragment is an alignment padded with no-ops of `unit` bytes. If
    /// the padding is not a multiple of that, the odd bytes are data: `$d`
    /// at the start, and `name` after them.
    OddPadding { unit: u64 },
}

/// A mapping symbol in the finished object.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MappingSymbol {
    pub section: SectionId,
    /// Offset within the section.
    pub offset: u64,
    pub name: &'static str,
}

impl Assembler {
    /// The current backend's mapping symbols for its code and for data, if
    /// the target has any and the object being written is one that has them.
    fn mapping_names(&self) -> Option<(&'static str, u64, &'static str)> {
        self.mapping_names_for(&self.arch_state)
    }

    /// [`Self::mapping_names`] for another instruction-set state, which the
    /// no-ops padding a section's tail are in.
    pub(crate) fn mapping_names_for(
        &self,
        state: &ArchState,
    ) -> Option<(&'static str, u64, &'static str)> {
        if matches!(
            self.options.format,
            crate::output::Format::Coff | crate::output::Format::MachO
        ) {
            return None;
        }
        mapping_names(self.arch.as_ref(), state)
    }

    /// Whether the current section is one GNU as keeps mapping symbols out of
    /// when they come from fragments: debug information is not code or data
    /// that anything disassembles.
    fn is_debug_section(&self) -> bool {
        self.interner
            .get(self.section(self.cur).name)
            .starts_with(".debug")
    }

    /// An instruction is about to be emitted: GNU as's `mapping_state` for
    /// code.
    pub(crate) fn map_code(&mut self) {
        let Some((code, align, _)) = self.mapping_names() else {
            return;
        };
        let s = self.section_mut(self.cur);
        if s.map_state == Some(code) {
            return;
        }
        s.align = s.align.max(align);
        self.map_transition(code, false);
    }

    /// Data is about to be emitted by a directive: GNU as's `mapping_state`
    /// for data.
    pub(crate) fn map_data(&mut self) {
        let Some((_, _, data)) = self.mapping_names() else {
            return;
        };
        let s = self.section(self.cur);
        // Data at the start of a section waits to be marked until code
        // follows, which marks it from the start.
        if s.map_state == Some(data) || s.map_state.is_none() {
            return;
        }
        self.map_transition(data, false);
    }

    /// An alignment padded with zeros, a `.space` or a `.fill` is about to be
    /// emitted: GNU as's `mapping_state_2` for data, called as the fragment
    /// is made.
    pub(crate) fn map_data_frag(&mut self) {
        let Some((_, _, data)) = self.mapping_names() else {
            return;
        };
        if !self.is_debug_section() {
            self.map_transition(data, false);
        }
    }

    /// An alignment padded with no-ops is about to be pushed as the next
    /// fragment.
    pub(crate) fn map_code_align(&mut self, state: &ArchState) {
        let Some(names) = self.mapping_names_for(state) else {
            return;
        };
        if !self.is_debug_section() {
            self.map_align_with(self.cur, names);
        }
    }

    /// [`Self::map_code_align`] for `section`, with the mapping names given.
    pub(crate) fn map_align_with(
        &mut self,
        section: SectionId,
        (code, unit, data): (&'static str, u64, &'static str),
    ) {
        let saved = std::mem::replace(&mut self.cur, section);
        self.map_transition_with(code, data, false);
        let s = self.section_mut(section);
        let frag = s.next_frag_index();
        s.map_events.push(MapEvent {
            frag,
            name: code,
            kind: MapEventKind::OddPadding { unit },
        });
        self.cur = saved;
    }

    /// GNU as's `mapping_state_2`: switches the current section to `name`,
    /// marking the change; or, with `always`, marks it even if the state
    /// already is `name`, as a literal pool does.
    pub(crate) fn map_transition(&mut self, name: &'static str, always: bool) {
        if let Some((_, _, data)) = self.mapping_names() {
            self.map_transition_with(name, data, always);
        }
    }

    /// [`Self::map_transition`], with the name of the data mapping symbol.
    fn map_transition_with(&mut self, name: &'static str, data: &'static str, always: bool) {
        let s = self.section_mut(self.cur);
        if s.map_state == Some(name) && !always {
            return;
        }
        // Code after unmarked data marks that data from the section start.
        if s.map_state.is_none() && name != data && !s.frags.is_empty() {
            s.map_events.push(MapEvent {
                frag: 0,
                name: data,
                kind: MapEventKind::At,
            });
        }
        s.map_state = Some(name);
        s.seal();
        let frag = s.next_frag_index();
        s.map_events.push(MapEvent {
            frag,
            name,
            kind: MapEventKind::At,
        });
    }

    /// Places every recorded mapping symbol, now that the layout is final.
    pub(crate) fn place_mapping_symbols(&mut self) {
        let mut out = Vec::new();
        for (si, s) in self.sections.iter().enumerate() {
            if s.map_events.is_empty() {
                continue;
            }
            let data = self.frag_arch(si, 0).0.data_mapping();
            let start = |frag: u32| s.frags.get(frag as usize).map_or(s.size, |f| f.offset);
            let mut placed: Vec<(u64, &'static str)> = Vec::new();
            for ev in &s.map_events {
                let at = start(ev.frag);
                match ev.kind {
                    MapEventKind::At => placed.push((at, ev.name)),
                    MapEventKind::OddPadding { unit } => {
                        let pad = s.frags.get(ev.frag as usize).map_or(0, |f| f.size());
                        let odd = pad % unit.max(1);
                        if odd != 0 {
                            placed.push((at, data));
                            placed.push((at + odd, ev.name));
                        }
                    }
                }
            }
            // A symbol followed by another at the same address says nothing,
            // and neither does one at the end of the section.
            let mut later = std::collections::HashSet::new();
            let mut kept = Vec::new();
            for &(at, name) in placed.iter().rev() {
                if later.insert(at) && at != s.size {
                    kept.push(MappingSymbol {
                        section: s.id,
                        offset: at,
                        name,
                    });
                }
            }
            kept.reverse();
            out.extend(kept);
        }
        self.mapping_symbols = out;
    }
}

/// `arch`'s mapping symbols for its code in `state` and for data.
pub(crate) fn mapping_names(
    arch: &dyn Architecture,
    state: &ArchState,
) -> Option<(&'static str, u64, &'static str)> {
    let (code, align) = arch.code_mapping(state)?;
    Some((code, align, arch.data_mapping()))
}
