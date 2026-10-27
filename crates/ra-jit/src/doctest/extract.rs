//! The examples in the documentation of a crate, found as rustdoc finds them: every item with
//! documentation is asked for its Markdown, and every code block of it that is Rust is one.

use std::path::PathBuf;

use hir::{Adt, AssocItem, HasAttrs, HasSource, Module, ModuleDef};
use ide_db::{RootDatabase, base_db::SourceDatabase, rust_doc::is_rust_fence};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use syntax::{AstNode, SyntaxKind, TextRange, TextSize, ast::HasName};

/// One code block of the documentation that is a test.
pub(crate) struct Example {
    /// The path of the documented item in its crate: empty for the crate itself.
    pub(crate) item: String,
    /// The file the documentation is written in.
    pub(crate) file: PathBuf,
    /// The line the block starts on, as rustdoc counts it.
    pub(crate) line: usize,
    pub(crate) marks: Marks,
    /// The code with its hidden lines as code.
    pub(crate) code: String,
}

/// What the line that opens a block says of it.
#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Marks {
    /// `ignore`, or `ignore-` and a part of the target's triple, which are in `ignored_on`.
    pub(crate) ignore: bool,
    pub(crate) ignored_on: Vec<String>,
    pub(crate) no_run: bool,
    pub(crate) should_panic: bool,
    pub(crate) compile_fail: bool,
    /// Whether the block has its own `#[test]` functions for the test harness to run.
    pub(crate) test_harness: bool,
    pub(crate) edition: Option<String>,
    /// The errors rustc has to give for a block that is to fail, like `E0308`.
    pub(crate) error_codes: Vec<String>,
}

impl Marks {
    /// `None` for a block that is not Rust.
    pub(crate) fn parse(lang: &str) -> Option<Marks> {
        let tokens = || lang.split([',', ' ', '\t']).map(str::trim).filter(|it| !it.is_empty());
        // `ignore-<target>` marks a block as Rust the way `ignore` does.
        let plain: Vec<&str> =
            tokens().map(|it| if it.starts_with("ignore-") { "ignore" } else { it }).collect();
        if !is_rust_fence(&plain.join(",")) {
            return None;
        }
        let mut marks = Marks::default();
        for token in tokens() {
            match token {
                "ignore" => marks.ignore = true,
                "no_run" => marks.no_run = true,
                "should_panic" => marks.should_panic = true,
                "compile_fail" => marks.compile_fail = true,
                "test_harness" => marks.test_harness = true,
                _ => {
                    if let Some(on) = token.strip_prefix("ignore-") {
                        marks.ignored_on.push(on.to_owned());
                    } else if let Some(edition) = token.strip_prefix("edition") {
                        marks.edition = Some(edition.to_owned());
                    } else if is_error_code(token) {
                        marks.error_codes.push(token.to_owned());
                    }
                }
            }
        }
        Some(marks)
    }
}

fn is_error_code(token: &str) -> bool {
    token.len() == 5 && token.starts_with('E') && token[1..].parse::<u32>().is_ok()
}

/// The examples of the crate, in the order of a walk of its modules.
pub(crate) fn examples(db: &RootDatabase, krate: hir::Crate) -> Vec<Example> {
    let whole_files = krate.edition(db).at_least_2024();
    let mut found = Found { db, examples: Vec::new(), seen: Default::default(), whole_files };
    found.module(krate.root_module(db), "");
    found.examples
}

struct Found<'db> {
    db: &'db RootDatabase,
    examples: Vec<Example>,
    /// Where the blocks found are: a macro that is exported is in two modules.
    seen: rustc_hash::FxHashSet<(PathBuf, usize, String)>,
    /// Whether the crate is of an edition from which rustdoc has the text of a file that
    /// `include_str!` reads be in that file.
    whole_files: bool,
}

fn join(path: &str, name: impl std::fmt::Display) -> String {
    if path.is_empty() { name.to_string() } else { format!("{path}::{name}") }
}

impl Found<'_> {
    fn module(&mut self, module: Module, path: &str) {
        let db = self.db;
        self.documented(module, path);
        for def in module.declarations(db) {
            let named = |name: hir::Name| join(path, name.as_str());
            match def {
                ModuleDef::Module(it) => {
                    if let Some(name) = it.name(db) {
                        self.module(it, &named(name));
                    }
                }
                ModuleDef::Function(it) => self.documented(it, &named(it.name(db))),
                ModuleDef::Adt(it) => self.adt(it, &named(it.name(db))),
                // One without a name is `_` to rustdoc.
                ModuleDef::Const(it) => match it.name(db) {
                    Some(name) => self.documented(it, &named(name)),
                    None => self.documented(it, &join(path, "_")),
                },
                ModuleDef::Static(it) => self.documented(it, &named(it.name(db))),
                ModuleDef::TypeAlias(it) => self.documented(it, &named(it.name(db))),
                ModuleDef::Trait(it) => {
                    let path = named(it.name(db));
                    self.documented(it, &path);
                    self.items(it.items(db), &path);
                }
                // One of `macro_rules!` is told of where it is written, below.
                // And the examples of a procedural one are those of its function, which may
                // have another name.
                ModuleDef::Macro(it) if !is_macro_rules(db, it) && !it.is_proc_macro() => {
                    self.documented(it, &named(it.name(db)))
                }
                _ => {}
            }
        }
        // A macro is in every module after it: its documentation is where it is written.
        for it in module.legacy_macros(db).into_iter().filter(|it| is_written_in(db, *it, module)) {
            self.documented(it, &join(path, it.name(db).as_str()));
        }
        for it in module.extern_blocks(db) {
            self.documented(it, path);
        }
        // rustdoc tests the examples of a `use` once, under the name it brings in, or under
        // none for a list or a glob (rustdoc of nightly-2026-10-25).
        for it in module.use_decls(db) {
            let name = it.source(db).value.use_tree().and_then(|tree| use_name(&tree));
            self.documented(it, &name.map_or_else(|| path.to_owned(), |name| join(path, name)));
        }
        for it in module.extern_crate_decls(db) {
            let name = it.alias_or_name(db);
            self.documented(it, &join(path, name.as_ref().map_or("_", |it| it.as_str())));
        }
        for it in module.impl_defs(db) {
            let Some(source) = it.source(db) else { continue };
            let Some(ty) = source.value.self_ty() else { continue };
            let path = join(path, shown_type(&ty));
            self.documented(it, &path);
            self.items(it.items(db), &path);
        }
    }

    fn items(&mut self, items: Vec<AssocItem>, path: &str) {
        for item in items {
            if let Some(name) = item.name(self.db) {
                self.documented(item, &join(path, name.as_str()));
            }
        }
    }

    fn adt(&mut self, adt: Adt, path: &str) {
        let db = self.db;
        self.documented(adt, path);
        let fields = match adt {
            Adt::Struct(it) => it.fields(db),
            Adt::Union(it) => it.fields(db),
            Adt::Enum(it) => {
                for variant in it.variants(db) {
                    let path = join(path, variant.name(db).as_str());
                    self.documented(variant, &path);
                    self.fields(variant.fields(db), &path);
                }
                Vec::new()
            }
        };
        self.fields(fields, path);
    }

    fn fields(&mut self, fields: Vec<hir::Field>, path: &str) {
        for field in fields {
            self.documented(field, &join(path, field.name(self.db).as_str()));
        }
    }

    /// Takes the examples out of the documentation of `item`, when it has any.
    fn documented(&mut self, item: impl HasAttrs, path: &str) {
        let Some(docs) = item.hir_docs(self.db) else { return };
        let text = docs.docs();
        // rustdoc counts the lines of the text from the line the documentation starts on. That
        // is not always the line the block is written on: the first line of a `/** */` that
        // has nothing is not in the text, nor is a `doc` of a `cfg_attr` that is off.
        let Some((file, first)) = self.start(docs) else { return };
        for block in blocks(text) {
            let Some(marks) = Marks::parse(&block.lang) else { continue };
            let place = (file.clone(), first + text[..block.start].matches('\n').count());
            if !self.seen.insert((place.0.clone(), place.1, path.to_owned())) {
                continue;
            }
            self.examples.push(Example {
                item: path.to_owned(),
                file: place.0,
                line: place.1,
                marks,
                code: with_hidden_lines(&block.code),
            });
        }
    }

    fn path(&self, file: ide_db::base_db::FileId) -> Option<PathBuf> {
        let root = self.db.file_source_root(file).source_root_id(self.db);
        let root = self.db.source_root(root).source_root(self.db);
        Some(PathBuf::from(root.path_for_file(&file)?.as_path()?.to_string()))
    }

    /// The file the documentation is written in and the line its first comment or attribute
    /// starts on.
    fn start(&self, docs: &hir::Docs) -> Option<(PathBuf, usize)> {
        // The first character and not the place before it: with nothing written at `mod name;`
        // that place is in neither file.
        let at = |offset: usize| TextRange::at(TextSize::new(offset as u32), TextSize::new(1));
        if let Some(written) = self.written(docs, at(0)) {
            return Some(written);
        }
        // The text starts with what a macro wrote, like `include_str!`.
        let (call, _) = docs.macro_calls().next()?;
        let call = call.to_in_file_node(self.db);
        let included = self.whole_files.then(|| included(&call.value)).flatten();
        let call = call.syntax().original_file_range_rooted(self.db);
        if let Some(included) = included {
            // From edition 2024 the text of a file that is included is in that file, and the
            // lines are its lines. When the documentation ends in a comment or an attribute
            // that is written, rustdoc counts from the line that one starts on instead.
            let text = docs.docs();
            let last = text.char_indices().rev().find(|(_, it)| !it.is_whitespace());
            if let Some(written) = last.and_then(|(it, _)| self.written(docs, at(it))) {
                return Some(written);
            }
            let path = self.path(call.file_id.file_id(self.db))?;
            return Some((path.parent()?.join(included), 1));
        }
        let tree = call.file_id.parse(self.db).syntax_node();
        let attr = tree.covering_element(call.range).ancestors();
        let attr = attr.into_iter().find(|it| it.kind() == SyntaxKind::ATTR);
        let start = attr.map_or(call.range, |it| it.text_range()).start();
        self.line_of(call.file_id, start)
    }

    /// The file and the line of the comment or attribute that the text at `range` of the
    /// documentation is written in, when it is written and not what a macro made.
    fn written(&self, docs: &hir::Docs, range: TextRange) -> Option<(PathBuf, usize)> {
        let db = self.db;
        let (range, _) = docs.find_ast_range(range)?;
        let tree = range.file_id.parse_or_expand(db);
        let token = tree.token_at_offset(range.value.start()).right_biased()?;
        // What a macro wrote of its own is where the macro is used, as rustdoc has it.
        let written = hir::InFile::new(range.file_id, token.clone()).original_file_range(db);
        let attr = token.parent_ancestors().find(|it| it.kind() == SyntaxKind::ATTR);
        // Before edition 2024 an attribute a macro wrote around what it was given is where
        // the macro is used, whatever line what is in it was written on.
        let pound = attr.as_ref().filter(|_| range.file_id.is_macro() && !self.whole_files);
        if let Some(pound) = pound.and_then(|it| it.first_token()) {
            let used = hir::InFile::new(range.file_id, pound).original_file_range(db);
            return self.line_of(used.file_id, used.range.start());
        }
        let attr = attr.filter(|_| !range.file_id.is_macro());
        let start = attr.map_or(written.range, |it| it.text_range()).start();
        self.line_of(written.file_id, start)
    }

    fn line_of(&self, file: hir::EditionedFileId, at: TextSize) -> Option<(PathBuf, usize)> {
        let file = file.file_id(self.db);
        let line = ide_db::line_index(self.db, file).line_col(at).line;
        Some((self.path(file)?, line as usize + 1))
    }
}

/// The file an `include_str!("file")` names, as it is written.
fn included(call: &syntax::ast::MacroCall) -> Option<String> {
    let includes = call.path()?.segment()?.name_ref().is_some_and(|it| it.text() == "include_str");
    let mut tokens = call.token_tree()?.syntax().descendants_with_tokens();
    let file =
        tokens.find_map(|it| <syntax::ast::String as syntax::AstToken>::cast(it.into_token()?))?;
    includes.then(|| file.value().ok().map(|it| it.into_owned())).flatten()
}

/// The name a `use` is tested under: the one it brings its path in as, and none for a list
/// or a glob.
fn use_name(tree: &syntax::ast::UseTree) -> Option<String> {
    if tree.use_tree_list().is_some() || tree.star_token().is_some() {
        return None;
    }
    if let Some(rename) = tree.rename() {
        return Some(rename.name().map_or("_".to_owned(), |it| it.text().to_string()));
    }
    tree.path().and_then(|it| it.segment()).map(|it| it.syntax().text().to_string())
}

fn is_macro_rules(db: &RootDatabase, it: hir::Macro) -> bool {
    let source = it.source(db).and_then(|it| it.value.left());
    matches!(source, Some(syntax::ast::Macro::MacroRules(_)))
}

/// Whether the macro is one of the items of the module as they are written: its own, or of a
/// macro that is used there.
fn is_written_in(db: &RootDatabase, it: hir::Macro, module: Module) -> bool {
    let Some(source) = it.source(db) else { return false };
    let written = source.syntax().original_file_range_rooted(db);
    let tree = written.file_id.parse(db).syntax_node();
    let around = tree.covering_element(written.range).ancestors();
    let inline = around.into_iter().find_map(syntax::ast::Module::cast);
    let inline = inline.map(|it| it.syntax().text_range());
    let module = module.definition_source(db);
    module.file_id == hir::HirFileId::from(written.file_id)
        && match &module.value {
            hir::ModuleSource::SourceFile(_) => inline.is_none(),
            hir::ModuleSource::Module(it) => inline == Some(it.syntax().text_range()),
            hir::ModuleSource::BlockExpr(_) => false,
        }
}

/// The type an `impl` is of as rustdoc names it: as the compiler writes it out, where a
/// reference has its lifetime even when none is written, and with no spaces.
fn shown_type(ty: &syntax::ast::Type) -> String {
    let mut shown = String::new();
    for token in ty.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
        if token.kind().is_trivia() {
            continue;
        }
        shown.push_str(token.text());
        let reference = token.parent().and_then(syntax::ast::RefType::cast);
        if token.kind() == SyntaxKind::AMP && reference.is_some_and(|it| it.lifetime().is_none()) {
            shown.push_str("'_");
        }
    }
    shown
}

/// A code block of a Markdown text.
struct Block {
    /// Where in the text it starts.
    start: usize,
    /// What follows the fence that opens it: nothing for an indented block.
    lang: String,
    code: String,
}

fn blocks(markdown: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION;
    let mut blocks = Vec::new();
    let mut open: Option<Block> = None;
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                open = Some(Block { start: range.start, lang, code: String::new() });
            }
            Event::Text(text) => {
                if let Some(block) = &mut open {
                    block.code.push_str(&text);
                }
            }
            Event::End(Tag::CodeBlock(_)) => blocks.extend(open.take()),
            _ => {}
        }
    }
    blocks
}

/// The code of a block as it is compiled: a line that starts with `# ` is not shown and is
/// code without it, and `##` is how a line that starts with `#` is written.
fn with_hidden_lines(code: &str) -> String {
    let line = |line: &str| {
        let trimmed = line.trim();
        if trimmed.starts_with("##") {
            line.replacen("##", "#", 1)
        } else if let Some(hidden) = trimmed.strip_prefix("# ") {
            hidden.to_owned()
        } else if trimmed == "#" {
            String::new()
        } else {
            line.to_owned()
        }
    };
    code.lines().map(line).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_of_a_block() {
        assert_eq!(Marks::parse(""), Some(Marks::default()));
        assert_eq!(Marks::parse("text"), None);
        assert_eq!(Marks::parse("sh,ignore"), None);
        let marks = Marks::parse("rust, should_panic,edition2018").unwrap();
        assert!(marks.should_panic && marks.edition.as_deref() == Some("2018"));
        let marks = Marks::parse("compile_fail,E0308").unwrap();
        assert!(marks.compile_fail && marks.error_codes == ["E0308"]);
        let marks = Marks::parse("ignore-wasm32").unwrap();
        assert!(!marks.ignore && marks.ignored_on == ["wasm32"]);
    }

    #[test]
    fn blocks_fenced_and_indented() {
        let found =
            blocks("Text.\n\n```no_run\nlet a = 1;\n```\n\n    let b = 2;\n\n~~~text\nc\n~~~\n");
        let found: Vec<_> = found.iter().map(|it| (it.lang.as_str(), it.code.as_str())).collect();
        assert_eq!(found, [("no_run", "let a = 1;\n"), ("", "let b = 2;\n"), ("text", "c\n")]);
    }

    #[test]
    fn type_of_an_impl_as_rustdoc_names_it() {
        let shown = |ty: &str| {
            let file = format!("impl<'a, R> T for {ty} {{}}");
            let file = syntax::ast::SourceFile::parse(&file, syntax::Edition::CURRENT).tree();
            let found = file.syntax().descendants().find_map(syntax::ast::Impl::cast);
            shown_type(&found.and_then(|it| it.self_ty()).unwrap())
        };
        assert_eq!(shown("&mut Deserializer<R>"), "&'_mutDeserializer<R>");
        assert_eq!(shown("&'a Map<K, V>"), "&'aMap<K,V>");
        assert_eq!(shown("IndexMap<K, V, S>"), "IndexMap<K,V,S>");
    }

    #[test]
    fn hidden_lines_are_code() {
        assert_eq!(
            with_hidden_lines("# let a = 1;\n  #\n##[derive(Debug)]\n#[x]\nb"),
            "let a = 1;\n\n#[derive(Debug)]\n#[x]\nb"
        );
    }
}
