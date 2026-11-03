//! Line information for the assembly source itself (`-g`, `--gdwarf-N`).
//!
//! Both references make a row for each instruction, at its line in the
//! source, and a compilation unit that describes the file; a numbered `.file`
//! means the source brings its own line table, and turns it off. How they
//! place the rows differs:
//!
//! - GNU as makes a row only where the line or file changes, in any code
//!   section. An instruction in a macro expansion is on the line that called
//!   the macro, one in a `.rept` or `.irp` on its line in the block, and one
//!   in an included file on its line in that file, which the file table names.
//! - llvm-mc makes a row for every instruction, in the first code section and
//!   those `.section` named as code afterwards, always in the main file: an
//!   instruction in any expansion is on the line of the outermost directive
//!   or call that expanded it. It also describes each label in the
//!   compilation unit. (An instruction on the last line of an included file
//!   is the exception: llvm-mc has returned to the including file by the time
//!   it looks the line up, and reads it from that file's buffer with a pointer
//!   into the other, so the number depends on where the two were allocated.
//!   rsasm gives the line in the included file.)

use super::{Flavor, Pos};
use crate::assembler::Assembler;
use crate::section::SectionId;
use crate::source::{FileId, Span};
use std::collections::HashMap;

/// Where an expansion's text came from.
#[derive(Copy, Clone, Debug)]
pub struct Expansion {
    /// The macro call or repeat directive.
    pub site: Span,
    /// For a `.rept`, `.irp` or `.irpc`: how many lines each copy of the
    /// block takes in the expansion, so a line can be mapped back into the
    /// block.
    pub copy_lines: Option<u32>,
}

/// What `-g` needs while the source is read.
#[derive(Default)]
pub struct GenState {
    /// Rows are being made for the source.
    pub on: bool,
    /// The file and line of the last row, which GNU as does not repeat.
    last: Option<(String, u32)>,
    /// The code sections llvm-mc describes, in the order it met them.
    pub sections: Vec<SectionId>,
    /// llvm-mc's label entries: name, line, position.
    pub labels: Vec<(String, u32, Pos)>,
    /// Expansions by the file their text was read as.
    pub expansions: HashMap<FileId, Expansion>,
    /// The file the assembly started with, which names the unit.
    pub main: Option<FileId>,
    /// llvm-mc has met a label or an instruction, anywhere, which is when it
    /// puts the source in a DWARF 4 file table.
    pub touched: bool,
}

impl Assembler {
    /// Starts generating rows, if the options ask for it. Called before the
    /// first statement.
    pub(crate) fn dwarf_start_generating(&mut self, main: FileId) {
        let st = &mut self.dwarf.line.source;
        if st.main.is_none() {
            st.main = Some(main);
            st.on = self.options.debug_source;
            // llvm-mc describes the section it starts in.
            st.sections.push(self.cur);
        }
    }

    /// A numbered `.file`: the source describes itself, so what was made up
    /// for it goes. GNU as drops the rows. llvm-mc forgets its file table
    /// and the compilation unit, but keeps the rows, still naming the file
    /// number the source had: 1, or 0 in DWARF 5, whatever that is now.
    pub(crate) fn dwarf_stop_generating(&mut self) {
        let st = &mut self.dwarf.line.source;
        if !st.on {
            return;
        }
        st.on = false;
        st.labels.clear();
        if self.dwarf_target().flavor == Flavor::Llvm {
            let file = if self.options.dwarf_version.unwrap_or(4) >= 5 {
                0
            } else {
                1
            };
            for (_, rows) in &mut self.dwarf.line.sequences {
                for row in rows.iter_mut().filter(|r| r.gen_path.is_some()) {
                    row.gen_path = None;
                    row.loc.file = file;
                }
            }
            return;
        }
        for (_, rows) in &mut self.dwarf.line.sequences {
            rows.retain(|r| r.gen_path.is_none());
        }
        self.dwarf
            .line
            .sequences
            .retain(|(_, rows)| !rows.is_empty());
        self.dwarf.line.rebuild_index();
    }

    /// Records the expansion `file` was made for.
    pub(crate) fn dwarf_expansion(&mut self, file: FileId, site: Span, copy_lines: Option<u32>) {
        self.dwarf
            .line
            .source
            .expansions
            .insert(file, Expansion { site, copy_lines });
    }

    /// A section switched to by `.section`: llvm-mc describes it too if it
    /// holds code.
    ///
    /// Darwin's `.section` does not reach here, and llvm-mc's does not
    /// register a section for `-g` either, so the unit in a Mach-O object
    /// describes the section the file started in and no other however much
    /// code the rest holds.
    pub(crate) fn dwarf_section_named(&mut self, id: SectionId) {
        let flags = self.section(id).flags;
        let st = &mut self.dwarf.line.source;
        if st.on && flags.alloc && flags.exec && !st.sections.contains(&id) {
            st.sections.push(id);
        }
    }

    /// The source file and line a statement at `span` is reported on, as
    /// the reference being followed reports it.
    fn source_line(&self, span: Span, flavor: Flavor) -> Option<(String, u32)> {
        let mut file = self.sm.lookup(span.lo)?;
        let mut line = file.line_col(span.lo).line;
        while let Some(exp) = self.dwarf.line.source.expansions.get(&file.id) {
            let site = self.sm.lookup(exp.site.lo)?;
            let site_line = site.line_col(exp.site.lo).line;
            line = match (flavor, exp.copy_lines) {
                // A line of a repeated block is that line of the block, which
                // starts on the line after the directive.
                (Flavor::Gnu, Some(n)) if n > 0 => site_line + 1 + (line - 1) % n,
                _ => site_line,
            };
            file = site;
        }
        Some((file.name.to_string_lossy().into_owned(), line))
    }

    /// Makes a row for an instruction about to be emitted as fragment `pos`.
    pub(crate) fn dwarf_source_row(&mut self, pos: Pos, back: Option<u32>, span: Span) {
        let flavor = self.dwarf_target().flavor;
        let Some((path, line)) = self.source_line(span, flavor) else {
            return;
        };
        let current = &self.dwarf.line.current;
        let loc = super::line::Loc {
            file: 0,
            line,
            column: 0,
            is_stmt: true,
            basic_block: false,
            prologue_end: false,
            epilogue_begin: false,
            isa: if flavor == Flavor::Gnu {
                current.isa
            } else {
                0
            },
            discriminator: 0,
            view: None,
            span,
        };
        match flavor {
            Flavor::Gnu => {
                let key = (path.clone(), line);
                if self.dwarf.line.source.last.as_ref() == Some(&key) {
                    return;
                }
                self.dwarf.line.source.last = Some(key);
                let flags = self.section(pos.0).flags;
                if !(flags.alloc && flags.exec) {
                    return;
                }
            }
            Flavor::Llvm => {
                self.dwarf.line.source.touched = true;
                if !self.dwarf.line.source.sections.contains(&pos.0) {
                    return;
                }
            }
        }
        self.dwarf.line.push_row(super::line::Row {
            pos,
            loc,
            back,
            gen_path: Some(path),
        });
    }

    /// Records a label for llvm-mc's compilation unit.
    pub(crate) fn dwarf_source_label(&mut self, name: &str, span: Span) {
        let target = self.dwarf_target();
        if !self.dwarf.line.source.on || target.flavor != Flavor::Llvm {
            return;
        }
        self.dwarf.line.source.touched = true;
        if name.starts_with(target.private_prefix)
            || !self.dwarf.line.source.sections.contains(&self.cur)
        {
            return;
        }
        let Some(file) = self.sm.lookup(span.lo) else {
            return;
        };
        let line = file.line_col(span.lo).line;
        let name = name.strip_prefix('_').unwrap_or(name).to_string();
        let pos = self.dwarf_pos();
        self.dwarf.line.source.labels.push((name, line, pos));
    }
}
