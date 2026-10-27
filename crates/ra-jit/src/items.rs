//! `ra-jit items <project> [--out <file>] [--refs]`: every item of the checkout's crates by what it is
//! (`blaze-items`), one JSON object a line: its kind, crate and path, the name the daemon's
//! patch base knows it by today (its file and its place in the file's table of items), and its
//! `local`, `interface`, `code` and `deep` hashes. `harness/items.py` compares two commits by it.
//! `--refs` adds what each item's slots refer to, with the hash its code hash takes of each.

use std::{fmt::Write as _, io::Write as _, path::Path};

use anyhow::Context;
use blaze_items::{ItemId, Items};
use hir_def::{AdtId, HasModule, Lookup, MacroId};
use hir_expand::{HirFileId, MacroCallKind};
use ide_db::RootDatabase;
use rustc_hash::FxHashMap;
use span::ErasedFileAstId;

use crate::{Args, Parts, cargo_config, load, load_config, process_wide};

pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut project = None;
    let mut out = None;
    let mut refs = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = Some(args.next().context("--out needs a file")?),
            "--refs" => refs = true,
            _ if project.is_none() => project = Some(arg),
            _ => anyhow::bail!("unexpected argument {arg}"),
        }
    }
    let project = project.context("usage: ra-jit items <project> [--out <file>] [--refs]")?;
    let asked = [project.clone(), "--crate".into(), "items".into(), "--out".into(), "none".into()];
    let mut args = Args::parse(asked.into_iter())?;
    process_wide();
    let (db, vfs, checkouts) = load(
        &[Path::new(&project)],
        &cargo_config(&args),
        load_config(),
        args.selection.clone(),
        None,
        None,
    )?;
    let Parts { root, base, members, debug, .. } = checkouts.parts(&args.project)?;
    (args.checkout, args.base, args.members, args.debug) = (root, base, members, debug);
    let roots = [
        std::path::absolute(&project)?.display().to_string(),
        std::fs::canonicalize(&project)?.display().to_string(),
    ];
    let crates: Vec<_> = ide_db::base_db::all_crates(&db)
        .iter()
        .copied()
        .filter(|it| it.data(&db).origin.is_local())
        .collect();
    hir::attach_db(&db, || {
        let names = Names { db: &db, vfs: &vfs, roots: &roots };
        let Items { items, external, forms, hashes } =
            blaze_items::of_crates(&db, &crates, &|id| names.place(id));
        let mut text = String::new();
        // A crate's root file and the crates of the checkout it depends on: today's store
        // names every item of a crate anew when a file of it or of one of those changes.
        for &krate in &crates {
            let data = krate.data(&db);
            let root = names.path_of(data.root_file_id);
            let deps: Vec<String> = data
                .dependencies
                .iter()
                .filter(|it| it.crate_id.data(&db).origin.is_local())
                .map(|it| names.name_of(it.crate_id))
                .collect();
            let line =
                serde_json::json!({ "crate": names.name_of(krate), "root": root, "deps": deps });
            writeln!(text, "{line}").ok();
        }
        let mut kinds = FxHashMap::<&str, usize>::default();
        let mut missed = FxHashMap::<&str, usize>::default();
        for item in &items {
            *kinds.entry(item.id.kind()).or_default() += 1;
            if let Some(why) = item.missed {
                *missed.entry(why).or_default() += 1;
            }
            let line = serde_json::json!({
                "kind": item.id.kind(),
                "crate": names.crate_name(item.id),
                "path": names.path(item.id),
                "name": names.place(item.id),
                "local": hex(&item.local),
                "interface": hex(&item.interface),
                "code": item.code.as_ref().map(hex),
                "deep": hex(&item.deep),
                "missed": item.missed,
            });
            let mut line = line;
            if refs {
                let named: Vec<_> = item
                    .refs
                    .iter()
                    .map(|(id, hash)| format!("{} {}", names.place(*id), &hex(hash)[..12]))
                    .collect();
                line["refs"] = named.into();
            }
            writeln!(text, "{line}").ok();
        }
        match &out {
            Some(path) => std::fs::write(path, &text)?,
            None => std::io::stdout().write_all(text.as_bytes())?,
        }
        let mut kinds: Vec<_> = kinds.into_iter().collect();
        kinds.sort();
        let mut missed: Vec<_> = missed.into_iter().collect();
        missed.sort_by_key(|it| std::cmp::Reverse(it.1));
        eprintln!(
            "items: {} in {} crates ({kinds:?}), {external} of other crates referred to; \
             forms {forms:.1?}, hashes {hashes:.1?} ({:.2} us an item); missed: {missed:?}",
            items.len(),
            crates.len(),
            hashes.as_secs_f64() * 1e6 / items.len().max(1) as f64,
        );
        Ok(())
    })
}

fn hex(hash: &blaze_items::Hash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Names<'a> {
    db: &'a RootDatabase,
    vfs: &'a vfs::Vfs,
    /// The checkout, as it was named and as it is: names of its files are written below it.
    roots: &'a [String],
}

impl Names<'_> {
    fn crate_name(&self, id: ItemId) -> String {
        self.name_of(id.krate(self.db))
    }

    fn name_of(&self, krate: ide_db::base_db::Crate) -> String {
        krate
            .extra_data(self.db)
            .display_name
            .as_ref()
            .map_or_else(String::new, |it| it.to_string())
    }

    /// Where an item is written, as the daemon's stable names have it: its file, below the
    /// checkout when it is in it, and its place in the file's table of items.
    fn place(&self, id: ItemId) -> String {
        let db = self.db;
        let (file, ast) = match id {
            ItemId::Function(it) => erased(it.lookup(db).id),
            ItemId::Adt(AdtId::StructId(it)) => erased(it.lookup(db).id),
            ItemId::Adt(AdtId::EnumId(it)) => erased(it.lookup(db).id),
            ItemId::Adt(AdtId::UnionId(it)) => erased(it.lookup(db).id),
            ItemId::Trait(it) => erased(it.lookup(db).id),
            ItemId::Impl(it) => erased(it.lookup(db).id),
            ItemId::Const(it) => erased(it.lookup(db).id),
            ItemId::Static(it) => erased(it.lookup(db).id),
            ItemId::Alias(it) => erased(it.lookup(db).id),
            ItemId::Macro(MacroId::MacroRulesId(it)) => erased(it.lookup(db).id),
            ItemId::Macro(MacroId::Macro2Id(it)) => erased(it.lookup(db).id),
            ItemId::Macro(MacroId::ProcMacroId(it)) => erased(it.lookup(db).id),
        };
        let mut out = String::new();
        self.file(&mut out, file);
        write!(out, "#{}", ast.into_raw()).ok();
        out
    }

    fn path_of(&self, file: vfs::FileId) -> String {
        let path = self.vfs.file_path(file).to_string();
        let below = self.roots.iter().find_map(|root| path.strip_prefix(root.as_str()));
        below.map_or(path.clone(), |it| format!("~{it}"))
    }

    fn file(&self, out: &mut String, file: HirFileId) {
        match file {
            HirFileId::FileId(file) => out.push_str(&self.path_of(file.file_id(self.db))),
            HirFileId::MacroFile(call) => {
                let (at, suffix) = match &call.loc(self.db).kind {
                    MacroCallKind::FnLike { ast_id, .. } => {
                        ((ast_id.file_id, ast_id.value.erase()), "!".to_owned())
                    }
                    MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => (
                        (ast_id.file_id, ast_id.value.erase()),
                        format!("!derive {derive_attr_index:?} {derive_index}"),
                    ),
                    MacroCallKind::Attr { ast_id, .. } => {
                        ((ast_id.file_id, ast_id.value.erase()), "!attr".to_owned())
                    }
                };
                self.file(out, at.0);
                write!(out, "#{}{suffix}", at.1.into_raw()).ok();
            }
        }
    }

    /// The item's path, for reading: the modules it is in and its name.
    fn path(&self, id: ItemId) -> String {
        let db = self.db;
        let module = match id {
            ItemId::Function(it) => it.module(db),
            ItemId::Adt(it) => it.module(db),
            ItemId::Trait(it) => it.module(db),
            ItemId::Impl(it) => it.module(db),
            ItemId::Const(it) => it.module(db),
            ItemId::Static(it) => it.module(db),
            ItemId::Alias(it) => it.module(db),
            ItemId::Macro(it) => it.module(db),
        };
        let mut path: Vec<String> = hir::Module::from(module)
            .path_to_root(db)
            .into_iter()
            .rev()
            .filter_map(|it| it.name(db).map(|it| it.as_str().to_owned()))
            .collect();
        let name = match id {
            ItemId::Function(it) => Some(hir::Function::from(it).name(db)),
            ItemId::Adt(it) => Some(hir::Adt::from(it).name(db)),
            ItemId::Trait(it) => Some(hir::Trait::from(it).name(db)),
            ItemId::Impl(_) => None,
            ItemId::Const(it) => hir::Const::from(it).name(db),
            ItemId::Static(it) => Some(hir::Static::from(it).name(db)),
            ItemId::Alias(it) => Some(hir::TypeAlias::from(it).name(db)),
            ItemId::Macro(it) => Some(hir::Macro::from(it).name(db)),
        };
        path.push(name.map_or_else(|| "_".to_owned(), |it| it.as_str().to_owned()));
        path.join("::")
    }
}

fn erased<N: syntax::AstNode>(id: hir_expand::AstId<N>) -> (HirFileId, ErasedFileAstId) {
    (id.file_id, id.value.erase())
}
