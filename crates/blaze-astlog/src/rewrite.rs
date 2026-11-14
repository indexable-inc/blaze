//! A run's results: [`query`] gives the derived rows and the lints' findings, as data; [`edit`]
//! turns the rewrites' actions into blaze-intent's edits and applies them in one request.
//!
//! A text action edits its node's range in the view's file. A node in a macro's expansion is
//! edited at the range of the macro input it came from; one the macro made itself has no such
//! range, and is an error naming the macro call, unless the rule excluded it
//! (`(not (in-macro-expansion n))`). Two rules' edits of overlapping text are an error naming
//! both and the place; the same edit twice is one edit. An absolute path written in a template
//! (`::std::sync::Arc`) is a def path, written as the edited node's module names that item
//! (`Module::find_path`), so the code resolves where it lands.

use std::alloc::Global;

use std::cmp::Ordering;

use astlog_engine::{Facts as _, PlannedAction, Segment, Severity, TextKind, Value, row_binding};
use base_db::Db;
use blaze_api::Edit;
use blaze_api::query::{self as api, Cell, Finding, Relation, Rows};
use blaze_hash::HashMap;
use blaze_intent::{Applied, Files, TextEdit};
use hir::{FindPathConfig, ModuleDef};
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use crate::error::{Error, OverlapSnafu, SourceError, UnplaceableSnafu, ViewTextSnafu};
use crate::program::{Action, Program};
use crate::sema::{Node, Sema};
use crate::splice::{self, Parens};

type Res<T> = Result<T, Error>;

/// Run `rules` over the view and return what it derives, as the API's
/// [`blaze_api::Response::Rows`] carries it: relations by name, each's rows by place, then the
/// findings by place. Rewrites are not planned.
///
/// # Errors
///
/// The rules' load errors, and a relation's failure, each naming its rules line.
pub fn query<F: Files>(db: &Db, files: &F, rules: &str) -> Res<Rows> {
    let program = Program::parse(rules)?;
    let facts = Sema::new(db, files)?;
    let rewrites = Program { rules: program.rules, rewrites: Vec::new(), lints: program.lints };
    let (database, _) = blaze_query::db::attach(db, || astlog_engine::run(&rewrites, &facts))?;
    let mut relations = Vec::new(); // alloc: one per relation, the answer's own
    for (name, relation) in &database.relations {
        let mut rows: Vec<Vec<Cell>> = relation
            .rows()
            .iter()
            .map(|row| row.iter().map(|value| cell(&facts, value)).collect::<Res<Vec<Cell>>>())
            .collect::<Res<_>>()?;
        rows.sort_by(|a, b| by_place(a, b));
        relations.push(Relation {
            name: name.clone(),
            rows,
            columns: relation.columns.clone(),
        });
    }
    let mut findings = Vec::new(); // alloc: one per lint row, the answer's own
    for lint in &rewrites.lints {
        let Some(relation) = database.relations.get(&lint.relation) else { continue };
        for row in relation.rows() {
            let binding = row_binding(&relation.columns, row);
            let message = astlog_engine::render(&facts, &lint.message, &binding)?;
            let mut at = None;
            for value in row {
                let it = cell(&facts, value)?;
                if matches!(it, Cell::Code { .. } | Cell::Expanded { .. }) {
                    at = Some(it);
                    break;
                }
            }
            findings.push(Finding {
                at,
                message,
                relation: lint.relation.clone(),
                severity: match lint.severity {
                    Severity::Error => api::Severity::Error,
                    Severity::Warning => api::Severity::Warning,
                },
            });
        }
    }
    findings.sort_by(|a, b| {
        by_place(a.at.as_slice(), b.at.as_slice()).then_with(|| a.message.cmp(&b.message))
    });
    Ok(Rows { findings, relations })
}

/// Rows in reading order: code by file, then offset, before every other kind of cell.
fn by_place(a: &[Cell], b: &[Cell]) -> Ordering {
    let mut b = b.iter();
    for a in a {
        let Some(b) = b.next() else { return Ordering::Greater };
        let order = place_key(a).cmp(&place_key(b));
        if order.is_ne() {
            return order;
        }
    }
    match b.next() {
        Some(_) => Ordering::Less,
        None => Ordering::Equal,
    }
}

/// A cell's kind, then its fields in reading order.
fn place_key(cell: &Cell) -> (u8, &str, u32, u32, &str, i64) {
    match cell {
        Cell::Code { end, path, text, start, .. } => (0, path, *start, *end, text, 0),
        Cell::Expanded { call, text } => (1, call, 0, 0, text, 0),
        Cell::Def { path } => (2, path, 0, 0, "", 0),
        Cell::Type { text } => (3, text, 0, 0, "", 0),
        Cell::Text(text) => (4, text, 0, 0, "", 0),
        Cell::Int(int) => (5, "", 0, 0, "", *int),
    }
}

/// Run `rules` over the view and apply every rewrite's actions, with `edits` before them, as
/// one request.
///
/// # Errors
///
/// A query's errors; an action that can't be placed or that overlaps another rule's, naming
/// both and the place; the edits' own failure, a file left unparsable among them.
pub fn edit<F: Files>(db: &Db, files: &F, rules: &str, edits: &[Edit]) -> Res<Applied> {
    edit_with(db, files, rules, edits, Parens::ByPrecedence)
}

/// [`edit`], with templates parenthesized by precedence or, for a test's control, not.
pub(crate) fn edit_with<F: Files>(
    db: &Db,
    files: &F,
    rules: &str,
    edits: &[Edit],
    parens: Parens,
) -> Res<Applied> {
    let program = Program::parse(rules)?;
    let facts = Sema::new(db, files)?;
    blaze_query::db::attach(db, || {
        let (_, planned) = astlog_engine::run(&program, &facts)?;
        let literal = literal_paths(&facts, &program)?;
        let mut text = Vec::new(); // alloc: the text edits of one request
        let mut more: Vec<Edit> = edits.to_vec();
        for planned in planned {
            let by = Origin { rule: planned.rule, line: planned.line };
            match planned.action {
                PlannedAction::Text { kind, node, template, binding, .. } => {
                    let paths = literal.get(planned.rule).map_or(&[][..], Vec::as_slice);
                    let written = Written { template, binding: &binding, parens };
                    let edit = text_edit(&facts, by, kind, node, written, paths)?;
                    text.push(Made { edit, by });
                }
                PlannedAction::Source { value, args, .. } => {
                    source_edit(&facts, by, *value, &args, &mut text, &mut more)?;
                }
            }
        }
        let text = without_overlap(files, text)?;
        blaze_intent::apply_with_text(db, files, &more, &text).map_err(|error| {
            SourceError::Edit { what: error.what.clone(), at: error.at.clone() }.into()
        })
    })
}

#[derive(Clone, Copy)]
struct Origin<'p> {
    rule: &'p str,
    line: usize,
}

impl Origin<'_> {
    fn by(self) -> String {
        format!("{} (rules:{})", self.rule, self.line)
    }
}

/// Where a syntax value is in the view: its file and range, mapped out of a macro's expansion
/// to its input; `Err` with the macro call's place for a node the macro made.
fn locate<'a, F: Files>(facts: &Sema<'a, F>, node: ast::Node<'a>) -> Result<(String, TextRange), String> {
    let tree = facts.tree_at(facts.tree(node));
    let Some(path) = tree.path.clone() else {
        return Err("(outside the view)".to_owned());
    };
    let Some(call) = tree.call.and_then(|it| facts.view_of(it)) else {
        return Ok((path, node.text_range()));
    };
    if let Some(range) = facts.sema.original_range_opt(node)
        && let Some(path) = facts.files.path(range.file_id.file_id(facts.db))
    {
        return Ok((path.to_owned(), range.range));
    }
    let (path, range) = locate(facts, call)?;
    Err(place(facts.files, &path, range.start()))
}

/// The 1-based line of an offset of a view file's text, when the view has the file.
fn line_of<F: Files>(files: &F, path: &str, at: TextSize) -> Option<u32> {
    let (_, bytes) = files.file(path)?;
    let before = bytes.get(..usize::from(at))?;
    // Fewer newlines than the `u32` offset `at`, so the count stays in `u32`.
    Some(memchr::memchr_iter(b'\n', before).fold(1, |line, _| line + 1))
}

/// `path:line` of an offset of a view file's text; the path alone when the view lacks it.
fn place<F: Files>(files: &F, path: &str, at: TextSize) -> String {
    match line_of(files, path, at) {
        Some(line) => format!("{path}:{line}"),
        None => path.to_owned(),
    }
}

/// The file's own text at `range`: a node of an expansion, mapped to the input it came from,
/// has the expansion's spacing, not the file's.
fn file_text<F: Files>(files: &F, path: &str, range: TextRange) -> Res<String> {
    let bytes = files
        .file(path)
        .and_then(|(_, bytes)| bytes.get(usize::from(range.start())..usize::from(range.end())));
    match bytes.map(std::str::from_utf8) {
        Some(Ok(text)) => Ok(text.to_owned()),
        Some(Err(_)) | None => ViewTextSnafu {
            place: place(files, path, range.start()),
            start: u32::from(range.start()),
            end: u32::from(range.end()),
        }
        .fail()
        .map_err(Error::from),
    }
}

fn cell<F: Files>(facts: &Sema<'_, F>, value: &Value<Node>) -> Res<Cell> {
    Ok(match value {
        Value::Text(text) => Cell::Text(text.to_string()),
        Value::Int(int) => Cell::Int(*int),
        Value::Node(node @ Node::Def(_)) => Cell::Def { path: facts.text(*node).into_owned() },
        Value::Node(node @ Node::Ty(_)) => Cell::Type { text: facts.text(*node).into_owned() },
        Value::Node(Node::Syn { tree, id }) => {
            let view = facts.syn_at(*tree, *id);
            match locate(facts, view) {
                Ok((path, range)) => {
                    let line = line_of(facts.files, &path, range.start());
                    let text = file_text(facts.files, &path, range)?;
                    Cell::Code {
                        end: u32::from(range.end()),
                        line,
                        path,
                        text,
                        start: u32::from(range.start()),
                    }
                }
                Err(call) => Cell::Expanded { call, text: view.text().to_owned() },
            }
        }
    })
}

fn unplaceable(by: Origin<'_>, place: String, message: impl Into<String>) -> Error {
    UnplaceableSnafu { place, rule: by.rule, line: by.line, message: message.into() }.build().into()
}

/// What a text action writes: its template, the binding it renders, and how.
#[derive(Clone, Copy)]
struct Written<'w> {
    template: &'w astlog_engine::Template,
    binding: &'w astlog_engine::Binding<Node>,
    parens: Parens,
}

fn text_edit<F: Files>(
    facts: &Sema<'_, F>,
    by: Origin<'_>,
    kind: TextKind,
    node: Node,
    written: Written<'_>,
    paths: &[(String, ModuleDef)],
) -> Res<TextEdit> {
    let Some(view) = facts.view_of(node) else {
        return Err(unplaceable(by, "(a def or a type)".to_owned(), "edits a value that is not code"));
    };
    let (path, range) = locate(facts, view).map_err(|call| {
        unplaceable(
            by,
            call,
            format!(
                "`{}` is made by this macro and is no text of its input; exclude it with \
                 (not (in-macro-expansion n))",
                view.text()
            ),
        )
    })?;
    let at = || place(facts.files, &path, range.start());
    let Written { template, binding, parens } = written;
    let rendered = splice::render(facts, template, binding, kind, view, parens)
        .map_err(|message| unplaceable(by, at(), message))?;
    let text = with_paths(facts, by, view, &path, range, &rendered, paths)?;
    let text = splice::in_place(kind, view, text, parens).map_err(|message| unplaceable(by, at(), message))?;
    let (range, text) = match kind {
        TextKind::Replace => (range, text),
        TextKind::Delete => (range, String::new()),
        TextKind::InsertBefore => (TextRange::empty(range.start()), text),
        TextKind::InsertAfter => (TextRange::empty(range.end()), text),
    };
    Ok(TextEdit { path, range, text, by: by.by() })
}

/// The def paths a rewrite's templates write literally whose first segment is a crate of the
/// build, each with the item it names, longest first.
///
/// # Errors
///
/// An absolute path that names no item, naming it and its rules line.
fn literal_paths<'p, F: Files>(
    facts: &Sema<'_, F>,
    program: &'p Program,
) -> Res<HashMap<&'p str, Vec<(String, ModuleDef)>, Global>> {
    let mut out = HashMap::new_in(Global);
    for rewrite in &program.rewrites {
        let mut found: Vec<(String, ModuleDef)> = Vec::new();
        for action in &rewrite.actions {
            let astlog_engine::Action::Text(text) = action else { continue };
            for segment in &text.template.segments {
                let Segment::Lit(lit) = segment else { continue };
                for path in def_paths(lit) {
                    if found.iter().any(|(it, _)| *it == path) {
                        continue;
                    }
                    // An absolute path starts at a crate: one that names nothing is a typo the
                    // rule would write into the code.
                    let defs = facts.items_at(&path, text.line)?;
                    if let Some(def) = defs.first() {
                        found.push((path, *def));
                    }
                }
            }
        }
        found.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        out.insert(rewrite.name.as_str(), found);
    }
    Ok(out)
}

/// The absolute `::a::b(::c)*` paths of `text`, leading `::` included. One after a name or a
/// `>` (`Vec::<u8>::new`) continues that path and is none.
fn def_paths(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let starts = bytes[at..].starts_with(b"::")
            && bytes.get(at + 2).is_some_and(|b| ident(*b) && !b.is_ascii_digit())
            && (at == 0 || !(ident(bytes[at - 1]) || matches!(bytes[at - 1], b':' | b'>' | b'.')));
        if !starts {
            at += 1;
            continue;
        }
        let mut end = at + 2;
        let mut segments = 0;
        loop {
            let start = end;
            while end < bytes.len() && ident(bytes[end]) {
                end += 1;
            }
            if end == start {
                break;
            }
            segments += 1;
            if bytes[end..].starts_with(b"::") && bytes.get(end + 2).is_some_and(|b| ident(*b)) {
                end += 2;
            } else {
                break;
            }
        }
        if segments >= 2 {
            out.push(text[at..end].to_owned());
        }
        at = end.max(at + 1);
    }
    out
}

/// `written` with each literal def path written as the module at `node` names its item.
fn with_paths<'a, F: Files>(
    facts: &Sema<'a, F>,
    by: Origin<'_>,
    node: ast::Node<'a>,
    path: &str,
    range: TextRange,
    written: &str,
    paths: &[(String, ModuleDef)],
) -> Res<String> {
    let found = def_paths(written);
    if paths.is_empty() || found.is_empty() {
        return Ok(written.to_owned());
    }
    let mut out = written.to_owned();
    for (literal, def) in paths {
        if !found.iter().any(|it| it == literal) {
            continue;
        }
        let named = named_at(facts, by, node, path, range, *def, literal)?;
        out = replace_path(&out, literal, &named);
    }
    Ok(out)
}

/// The path the module at `node` names `def` by.
fn named_at<'a, F: Files>(
    facts: &Sema<'a, F>,
    by: Origin<'_>,
    node: ast::Node<'a>,
    path: &str,
    range: TextRange,
    def: ModuleDef,
    literal: &str,
) -> Res<String> {
    let Some(scope) = facts.sema.scope(node) else {
        return Err(unplaceable(by, place(facts.files, path, range.start()), "is in no module"));
    };
    let config = FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: true,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let Some(named) = scope.module().find_path(facts.db, def, config) else {
        return Err(unplaceable(
            by,
            place(facts.files, path, range.start()),
            format!("this module can name no path to `{literal}`"),
        ));
    };
    Ok(named.display(facts.db, scope.krate().edition(facts.db)).to_string())
}

/// `text` with every whole occurrence of the path `from` replaced by `to`.
fn replace_path(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut before: Option<char> = None;
    while let Some(at) = rest.find(from) {
        let prior = rest[..at].chars().next_back().or(before);
        let after = rest[at + from.len()..].chars().next();
        let ident = |c: char| c.is_alphanumeric() || c == '_' || c == ':';
        out.push_str(&rest[..at]);
        if prior.is_some_and(ident) || after.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            out.push_str(from);
        } else {
            out.push_str(to);
        }
        before = from.chars().next_back();
        rest = &rest[at + from.len()..];
    }
    out.push_str(rest);
    out
}

fn source_edit<'p, F: Files>(
    facts: &Sema<'_, F>,
    by: Origin<'p>,
    action: Action,
    args: &[Value<Node>],
    text: &mut Vec<Made<'p>>,
    edits: &mut Vec<Edit>,
) -> Res<()> {
    let [target, Value::Text(to)] = args else {
        return Err(unplaceable(by, "(rules)".to_owned(), "takes a value and a string"));
    };
    let Value::Node(target) = target else {
        return Err(unplaceable(by, "(rules)".to_owned(), "takes a def or a node first"));
    };
    match action {
        Action::Rename => {
            let def = match facts.def_at(*target) {
                Some(def) => Some(def),
                None => facts.view_of(*target).and_then(|it| facts.defines(it).or_else(|| facts.resolve(it))),
            };
            let Some(def) = def else {
                return Err(unplaceable(by, "(rules)".to_owned(), "renames a value that is no def"));
            };
            let Some(source) = facts.source_of(def) else {
                return Err(unplaceable(
                    by,
                    facts.def_path(def).map_or_else(|| "(a def with no path)".to_owned(), |it| it),
                    "renames a def with no source in the view",
                ));
            };
            let (file, _) = locate(facts, source)
                .map_err(|call| unplaceable(by, call, "renames a def a macro declares"))?;
            let rename = Edit::Rename {
                to: to.to_string(),
                item: blaze_api::Target { file, item: blaze_intent::path::of(source) },
            };
            // Every row that reaches one def asks for its rename: the same rename is one edit.
            if !edits.contains(&rename) {
                edits.push(rename);
            }
        }
        Action::Retarget => {
            let Some(view) = facts.view_of(*target) else {
                return Err(unplaceable(by, "(rules)".to_owned(), "retargets a value that is not code"));
            };
            let Some(path) = path_node(view) else {
                return Err(unplaceable(
                    by,
                    locate(facts, view).map_or_else(|it| it, |(p, r)| place(facts.files, &p, r.start())),
                    format!("retargets `{}`, which is no path", view.text()),
                ));
            };
            let (file, range) = locate(facts, path).map_err(|call| {
                unplaceable(by, call, "retargets a path a macro made")
            })?;
            let def = facts.items_at(to, by.line)?;
            let Some(def) = def.first() else {
                return Err(unplaceable(by, place(facts.files, &file, range.start()), "names no item"));
            };
            let named = named_at(facts, by, path, &file, range, *def, to)?;
            // The path up to the item's name: its last segment's generic arguments stay.
            let Some(name) = ast::Path::cast(path).and_then(|it| it.segment()).and_then(|it| it.name_ref())
            else {
                return Err(unplaceable(
                    by,
                    place(facts.files, &file, range.start()),
                    format!("retargets `{}`, whose last segment is no name", path.text()),
                ));
            };
            let end = range.start() + (name.syntax().text_range().end() - path.text_range().start());
            text.push(Made {
                edit: TextEdit {
                    path: file,
                    range: TextRange::new(range.start(), end),
                    text: named,
                    by: by.by(),
                },
                by,
            });
        }
        Action::AddUse => {
            let Some(view) = facts.view_of(*target) else {
                return Err(unplaceable(by, "(rules)".to_owned(), "adds a use beside a value that is not code"));
            };
            let (file, _) =
                locate(facts, view).map_err(|call| unplaceable(by, call, "adds a use in a macro's output"))?;
            let into = view
                .ancestors()
                .find(|it| it.kind() == SyntaxKind::MODULE)
                .map(blaze_intent::path::of);
            edits.push(Edit::AddUse { file, into, path: to.to_string() });
        }
    }
    Ok(())
}

/// The path node a retarget edits: `node`, or the path of a path expression, type or pattern.
fn path_node(node: ast::Node<'_>) -> Option<ast::Node<'_>> {
    if node.kind() == SyntaxKind::PATH {
        return Some(node);
    }
    node.children().find(|it| it.kind() == SyntaxKind::PATH)
}

/// A text edit and the rule that made it.
struct Made<'p> {
    edit: TextEdit,
    by: Origin<'p>,
}

/// The text edits, each once, in file and offset order. Two of different text overlap when
/// one starts inside the other's range, or both insert at one offset (their order would be a
/// guess); that is an error naming both rules and the place.
fn without_overlap<F: Files>(files: &F, mut made: Vec<Made<'_>>) -> Res<Vec<TextEdit>> {
    let key = |it: &Made<'_>| {
        (it.edit.path.clone(), it.edit.range.start(), it.edit.range.end(), it.edit.text.clone())
    };
    made.sort_by_key(key);
    made.dedup_by(|b, a| a.edit.path == b.edit.path && a.edit.range == b.edit.range && a.edit.text == b.edit.text);
    for pair in made.windows(2) {
        let [a, b] = pair else { continue };
        let (x, y) = (a.edit.range, b.edit.range);
        let both_insert = x.is_empty() && y.is_empty() && x.start() == y.start();
        if a.edit.path == b.edit.path && (y.start() < x.end() || both_insert) {
            return OverlapSnafu {
                place: place(files, &a.edit.path, y.start()),
                first: a.by.rule,
                first_line: a.by.line,
                second: b.by.rule,
                second_line: b.by.line,
            }
            .fail()
            .map_err(Error::from);
        }
    }
    Ok(made.into_iter().map(|it| it.edit).collect())
}

