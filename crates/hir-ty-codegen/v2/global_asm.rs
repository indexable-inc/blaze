//! A crate's `global_asm!`s, assembled into the one object the link takes of them as it
//! takes a C object ([`HirFrontend::global_asm`]): every symbol, section and unwind entry
//! the assembly defines, with the relocations of what it names.
//!
//! The item tree keeps no item for a `global_asm!` (`hir_def::item_tree::lower`): the blocks
//! are found from the macro calls of the crate's modules, and each is lowered by
//! [`hir_def::expr_store::lower::global_asm`].

#[cfg(test)]
mod tests;

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_codegen::{CodegenError, InstanceKey};
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use hir_def::{
    expr_store::lower::{GlobalAsm, global_asm},
    hir::{AsmOptions, AsmPiece},
    nameres::crate_def_map,
};
use hir_expand::{MacroDefKind, builtin::BuiltinFnLikeExpander};
use hir_ty_mir::mir::location::line_and_column;

use super::frontend::HirFrontend;
use crate::{Arch, Target};

/// The object a crate's `global_asm!`s assemble to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalAsmObject {
    /// The hash of `bytes`: the link's name of the input, so an edit of a block is one
    /// object removed and one added.
    pub id: ContentId,
    /// What the link's errors call it: the `global_asm!`s of the crate, by its name.
    pub name: String,
    /// The object file, which the assembler keeps for the process: shared, never copied.
    pub bytes: Arc<[u8]>,
}

impl HirFrontend<'_> {
    /// The object `krate`'s `global_asm!`s assemble to for `triple`, or `None` when it has
    /// none. The blocks are in the order they are written in (by file, then place), each as
    /// rustc gives LLVM's assembler one: in Intel's syntax on x86-64 unless it says
    /// `options(att_syntax)`. A block blaze can't assemble is an error naming the crate, the
    /// file and line of the `global_asm!`, and the line of its template the assembler
    /// refused with what it said.
    pub fn global_asm(
        &self,
        krate: Crate,
        triple: &str,
    ) -> Result<Option<GlobalAsmObject>, CodegenError> {
        self.with(|db| object(db, krate, triple)).map_err(error)
    }
}

fn error(what: String) -> CodegenError {
    CodegenError {
        unit: InstanceKey(ContentId::default()),
        item: ItemKey(ContentId::default()),
        what,
        front: None,
    }
}

/// What the link's ID of a crate's `global_asm!` object is derived under.
const OBJECT_DOMAIN: &str = "blaze v2 global_asm object 1";

/// One `global_asm!` of a crate, rendered for the assembler.
struct Block {
    /// The file the `global_asm!` is written in, as errors show it, and where in it.
    path: String,
    offset: u32,
    line: u32,
    /// Its text, the template's lines and what blaze puts around them.
    text: String,
    /// How many lines of `text` come before the template's first.
    before: usize,
}

impl Block {
    /// The `global_asm!` as an error names it.
    fn named(&self, crate_name: &str) -> String {
        // alloc: an error's name of the block
        format!("the `global_asm!` at {}:{} of crate `{crate_name}`", self.path, self.line)
    }
}

fn object(db: &Db, krate: Crate, triple: &str) -> Result<Option<GlobalAsmObject>, String> {
    let crate_name = krate.named(db).to_string(); // alloc: the crate's name in the errors
    let target = Target::of(triple)?;
    let blocks = blocks(db, krate, &crate_name, target.arch)?;
    if blocks.is_empty() {
        return Ok(None);
    }
    if target.is_wasm() {
        // alloc: the error's message
        return Err(format!(
            "{}: blaze assembles `global_asm!` for x86-64 and AArch64, not for {triple}",
            blocks[0].named(&crate_name)
        ));
    }
    // alloc: once per crate with `global_asm!`, the text its object is assembled from
    let mut text = String::new();
    // The line each block's text starts at in `text`, from 0.
    // alloc: as above
    let mut starts = Vec::with_capacity(blocks.len());
    let mut lines = 0;
    for block in &blocks {
        starts.push(lines);
        text.push_str(&block.text);
        lines += block.text.lines().count();
    }
    // The triple as the code generator's assembler takes it, as `backend::assembled` gives it.
    let isa = crate::isa_for(target).map_err(|e| e.to_string())?; // alloc: the error's message
    // alloc: once per crate with `global_asm!`, the assembler takes the triple as text
    let bytes = crate::asm::object(&isa.triple().to_string(), &text).map_err(|said| {
        refused(&said, &text, &blocks, &starts, crate::asm::lines_before(target.arch), &crate_name)
    })?;
    Ok(Some(GlobalAsmObject {
        id: ContentId::of(OBJECT_DOMAIN, &bytes),
        // alloc: once per crate with `global_asm!`, the link's name of its object
        name: format!("the `global_asm!`s of crate `{crate_name}`"),
        bytes,
    }))
}

/// Every `global_asm!` of `krate`, rendered, in the order they are written in: by the file
/// and the place of the call, a call a macro made at the call of the macro written.
fn blocks(db: &Db, krate: Crate, crate_name: &str, arch: Arch) -> Result<Vec<Block>, String> {
    // alloc: per crate linked, its `global_asm!`s, none in most
    let mut blocks = Vec::new();
    let def_map = crate_def_map(db, krate);
    for (module, data) in def_map.modules(db) {
        for (_, &call) in data.scope(db).iter_macro_invoc() {
            let loc = call.loc(db);
            if !matches!(loc.def.kind, MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::GlobalAsm)) {
                continue;
            }
            let range = loc.kind.original_call_range_with_input(db);
            let (path, line) = match line_and_column(db, range) {
                Some((file, line, _)) => (file_path(db, krate, file), line),
                // alloc: an error's name of a place that has none
                None => ("<no file>".to_owned(), 0),
            };
            let offset = range.range.start().into();
            let mut block = Block { path, offset, line, text: String::new(), before: 0 };
            let Some(lowered) = global_asm(db, module, call) else {
                // alloc: the error's message
                return Err(format!(
                    "{}: its expansion is no `global_asm!` blaze reads",
                    block.named(crate_name)
                ));
            };
            render(&mut block, lowered, crate_name, arch)?;
            blocks.push(block);
        }
    }
    // Sorted so the object does not depend on the order of the macro calls' table.
    blocks.sort_by(|a, b| (&a.path, a.offset, &a.text).cmp(&(&b.path, b.offset, &b.text)));
    Ok(blocks)
}

/// The text of one `global_asm!`, as rustc gives it to LLVM: its template, and on x86-64
/// one that says `options(att_syntax)` in AT&T's syntax, which the assembler is switched to
/// and back from (it is given every block in Intel's, [`crate::asm::lines_before`]).
fn render(
    block: &mut Block,
    lowered: &GlobalAsm,
    crate_name: &str,
    arch: Arch,
) -> Result<(), String> {
    if let Some(operand) = lowered.operands.first() {
        // alloc: the error's message
        return Err(format!(
            "{} has a `{operand}` operand: blaze assembles a `global_asm!` whose template is \
             all text, with no operands",
            block.named(crate_name)
        ));
    }
    let Some(template) = &lowered.template else {
        // alloc: the error's message
        return Err(format!(
            "{}: a template string blaze could not read (a macro that expands to no string \
             literal), or a `{{..}}` that names no operand",
            block.named(crate_name)
        ));
    };
    let att = lowered.options.contains(AsmOptions::ATT_SYNTAX) && arch == Arch::X86_64;
    if att {
        block.text.push_str(".att_syntax\n");
        block.before = 1;
    }
    for piece in template {
        match piece {
            AsmPiece::Lit(lit) => block.text.push_str(lit),
            AsmPiece::Operand { index, .. } => {
                // alloc: the error's message
                return Err(format!(
                    "{}: its template names operand {index}, and it has none",
                    block.named(crate_name)
                ));
            }
        }
    }
    if !block.text.ends_with('\n') {
        block.text.push('\n');
    }
    if att {
        block.text.push_str(".intel_syntax noprefix\n");
    }
    Ok(())
}

/// The file `file` of `krate` as an error shows it: its path from the crate's root, or as
/// the source root has it when it is elsewhere.
fn file_path(db: &Db, krate: Crate, file: base_db::FileId) -> String {
    let root = db.file_source_root(file).source_root_id(db);
    let root = db.source_root(root).source_root(db);
    let Some(path) = root.path_for_file(&file) else {
        // alloc: an error's name of a file
        return format!("{file:?}");
    };
    let Some(path) = path.as_path() else {
        // alloc: an error's name of a file: a virtual one, as the source root has it
        return path.to_string();
    };
    let path = path.as_str();
    let workspace = krate.data(db).proc_macro_cwd.as_str();
    // alloc: an error's name of a file
    path.strip_prefix(workspace).map_or(path, |it| it.trim_start_matches('/')).to_owned()
}

/// The error of LLVM's assembler that refused `text`: each line it names (`<inline asm>:L:C:`,
/// `before` lines past the text's own) as the
/// block it is in, that line of its template and what was said of it. An error that names no
/// line is said of every block of the crate.
fn refused(
    said: &str,
    text: &str,
    blocks: &[Block],
    starts: &[usize],
    before: usize,
    crate_name: &str,
) -> String {
    let lines: Vec<&str> = text.lines().collect(); // alloc: error path
    // alloc: error path
    let mut out = Vec::new();
    for (line, message) in diagnostics(said) {
        let Some(line) = line.checked_sub(before + 1) else { continue };
        // The block the line is in: the last that starts at or before it.
        let at = starts.partition_point(|&start| start <= line).saturating_sub(1);
        let (block, start) = (&blocks[at], starts[at]);
        let of_template = (line - start).checked_sub(block.before).map_or(0, |it| it + 1);
        let written = lines.get(line).map_or("", |it| it.trim());
        // alloc: error path
        out.push(format!(
            "{}: line {of_template} of its template, `{written}`: {message}",
            block.named(crate_name)
        ));
    }
    if out.is_empty() {
        let named: Vec<String> = blocks.iter().map(|it| it.named(crate_name)).collect(); // alloc: error path
        // alloc: the error's message
        return format!("{}: {said}", named.join(", "));
    }
    out.join("; ")
}

/// Each line an assembler's error names, from 1, with what it said of it: the rest of its
/// line, without the `error:` before it.
fn diagnostics(said: &str) -> Vec<(usize, &str)> {
    // alloc: error path
    let mut found = Vec::new();
    for marker in ["<inline asm>:"] {
        let mut rest = said;
        while let Some(at) = rest.find(marker) {
            rest = &rest[at + marker.len()..];
            let mut fields = rest.splitn(3, ':');
            let (Some(line), Some(column), Some(message)) =
                (fields.next(), fields.next(), fields.next())
            else {
                break;
            };
            let (Ok(line), true) =
                (line.parse::<usize>(), column.chars().all(|it| it.is_ascii_digit()))
            else {
                continue;
            };
            // LLVM's errors are joined by `; ` (`llvm::Machine::with`).
            let message = message.split('\n').next().and_then(|it| it.split("; ").next());
            let message = message.unwrap_or("").trim();
            let message = message.strip_prefix("error:").map_or(message, str::trim);
            found.push((line, message));
        }
    }
    found.sort_by_key(|it| it.0);
    found
}
