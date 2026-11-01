//! The edits that need only the file's tree: each finds its target by path, makes one splice,
//! and says which items it touched and how. A rename needs the front end ([`crate::rename`]).

use blaze_api::{Edit, How};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, Node, Token},
};

use crate::{
    file::Working,
    path::{self, Kind},
};

/// Where an item an edit touched is.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum At {
    /// A top item of the base file, by where it starts there.
    Item(TextSize),
    /// The file's own module.
    File,
    /// An item the edit added, by its path as the edits left the file.
    Added(String),
}

/// An item an edit touched: in the request's `file`th file.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Touch {
    pub file: u32,
    pub at: At,
    pub how: How,
}

/// The file an edit works in, as the request names it; `None` for a replace, which works in
/// every file that names its item.
pub fn file_of(edit: &Edit) -> Option<&str> {
    match edit {
        Edit::ReplaceBody { item, .. }
        | Edit::ReplaceSignature { item, .. }
        | Edit::ReplaceItem { item, .. }
        | Edit::RemoveItem { item }
        | Edit::Rename { item, .. } => Some(&item.file),
        Edit::AddField { strukt, .. } | Edit::RemoveField { strukt, .. } => Some(&strukt.file),
        Edit::AddItem { file, .. } | Edit::AddUse { file, .. } => Some(file),
        Edit::Replace { .. } => None,
    }
}

/// Makes `edit`, which is no rename, in `file` (the request's `index`th), adding what it
/// touched to `out`.
pub fn apply(file: &mut Working, index: u32, edit: &Edit, out: &mut Vec<Touch>) -> Result<(), String> {
    let (delete, insert) = splice_of(file, index, edit, out)?;
    let inserted = TextRange::at(delete.start(), TextSize::of(insert.as_str()));
    file.splice(delete, insert)?;
    if let Edit::AddItem { .. } = edit {
        added(file, index, inserted, out);
    }
    Ok(())
}

/// The one splice `edit` makes in `file` as the edits before it left it, adding what it touches
/// to `out`.
fn splice_of(file: &Working, index: u32, edit: &Edit, out: &mut Vec<Touch>) -> Result<(TextRange, String), String> {
    let root = file.root();
    let found = |item: &str| path::find(root, item).map_err(|error| format!("{}: {error}", file.path));
    match edit {
        Edit::ReplaceBody { body, item } => {
            let node = expect(found(&item.item)?, &[Kind::Fn], &item.item)?;
            let trimmed = body.trim();
            if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
                return Err(format!("the body given for `{}` is no block `{{ … }}`", item.item));
            }
            let at = base_of(file, node)?;
            let function = ast::Fn::cast(node).expect("a fn node");
            match function.body() {
                Some(block) => {
                    out.push(Touch { file: index, at, how: How::Body });
                    Ok((block.syntax().text_range(), body.clone()))
                }
                None => {
                    let semicolon = function.semicolon_token().ok_or_else(|| {
                        format!("`{}` has neither a body nor a `;` to put one in place of", item.item)
                    })?;
                    out.push(Touch { file: index, at, how: How::Whole });
                    Ok((semicolon.text_range(), format!(" {body}")))
                }
            }
        }
        Edit::ReplaceSignature { item, signature } => {
            let node = expect(found(&item.item)?, &[Kind::Fn], &item.item)?;
            let start = first_own_token(node).ok_or_else(|| format!("`{}` is empty", item.item))?;
            let function = ast::Fn::cast(node).expect("a fn node");
            let end = match (function.body(), function.semicolon_token()) {
                (Some(block), _) => block.syntax().text_range().start(),
                (None, Some(semicolon)) => semicolon.text_range().start(),
                (None, None) => node.text_range().end(),
            };
            let end = trim_back(file.text(), start, end);
            out.push(Touch { file: index, at: base_of(file, node)?, how: How::Signature });
            Ok((TextRange::new(start, end), signature.clone()))
        }
        Edit::ReplaceItem { item, text } => {
            let node = found(&item.item)?;
            let top = path::top_of(node).expect("a path names a top item or what is in one");
            let how = if top == node { How::Whole } else { How::Fields };
            out.push(Touch { file: index, at: base_of(file, top)?, how });
            Ok((node.text_range(), text.clone()))
        }
        Edit::AddField { field, strukt } => {
            let node = expect(found(&strukt.item)?, &[Kind::Struct, Kind::Union, Kind::Variant], &strukt.item)?;
            let top = path::top_of(node).expect("a struct, union or variant is in a top item");
            out.push(Touch { file: index, at: base_of(file, top)?, how: How::Fields });
            add_field(file.text(), node, field)
        }
        Edit::RemoveField { field, strukt } => {
            let node = expect(found(&strukt.item)?, &[Kind::Struct, Kind::Union, Kind::Variant], &strukt.item)?;
            let target = path::members(node)
                .find(|it| path::name_of(*it).as_deref() == Some(field.as_str()))
                .ok_or_else(|| format!("{}: `{}` has no field `{field}`", file.path, strukt.item))?;
            let top = path::top_of(node).expect("a struct, union or variant is in a top item");
            out.push(Touch { file: index, at: base_of(file, top)?, how: How::Fields });
            Ok((remove_entry(file.text(), target), String::new()))
        }
        Edit::RemoveItem { item } => {
            let node = found(&item.item)?;
            let top = path::top_of(node).expect("a path names a top item or what is in one");
            if top != node {
                out.push(Touch { file: index, at: base_of(file, top)?, how: How::Fields });
                return Ok((remove_entry(file.text(), node), String::new()));
            }
            out.push(Touch { file: index, at: base_of(file, node)?, how: How::Removed });
            out.push(Touch { file: index, at: scope_at(file, node)?, how: How::Scope });
            Ok((remove_line(file.text(), file.end, node.text_range()), String::new()))
        }
        Edit::AddItem { file: _, into, text } => {
            let container = container(file, into.as_deref())?;
            let at = match container {
                Some(node) => base_of(file, node)?,
                None => At::File,
            };
            out.push(Touch { file: index, at, how: How::Scope });
            add_item(file.text(), file.end, container.unwrap_or(root), text)
        }
        Edit::AddUse { file: _, into, path: used } => {
            let used = used.trim();
            if used.is_empty() || used.ends_with(';') || used.starts_with("use ") {
                return Err(format!("`{used}` is no path to use: give it without `use` and `;`"));
            }
            let container = container(file, into.as_deref())?;
            if container.is_some_and(|it| it.kind() != SyntaxKind::MODULE) {
                return Err(format!("{}: a use goes in a module, and `{}` is none", file.path, into.as_deref().unwrap_or("")));
            }
            let scope = container.unwrap_or(root);
            let wanted = used.chars().filter(|it| !it.is_whitespace());
            let present = items_of(scope).any(|it| {
                ast::Use::cast(it).and_then(|it| it.use_tree()).is_some_and(|tree| {
                    tree.syntax().text().chars().filter(|it| !it.is_whitespace()).eq(wanted.clone())
                })
            });
            if present {
                return Err(format!("{}: `use {used};` is there already", file.path));
            }
            let at = match container {
                Some(node) => base_of(file, node)?,
                None => At::File,
            };
            out.push(Touch { file: index, at, how: How::Scope });
            add_use(file.text(), file.end, scope, used)
        }
        Edit::Rename { .. } => unreachable!("a rename is made by `crate::rename`"),
        Edit::Replace { .. } => unreachable!("a replace is made by `crate::replace`"),
    }
}

/// `node` when it is one of `kinds`, else an error naming what it is.
fn expect<'t>(node: Node<'t>, kinds: &[Kind], path: &str) -> Result<Node<'t>, String> {
    match Kind::of(node.kind()) {
        Some(kind) if kinds.contains(&kind) => Ok(node),
        kind => {
            let wanted: Vec<&str> = kinds.iter().map(|it| it.keyword()).collect();
            Err(format!(
                "`{path}` is a {}, and the edit takes a {}",
                kind.map_or("node", Kind::keyword),
                wanted.join(" or ")
            ))
        }
    }
}

/// The top item of the base tree `root` that starts at `offset`: found down from the root by
/// binary search of each level's children, then up to the item.
pub fn item_at(root: Node<'_>, offset: TextSize) -> Option<Node<'_>> {
    if offset >= root.text_range().end() {
        return None;
    }
    let first = match root.covering_element(TextRange::at(offset, 1.into())) {
        NodeOrToken::Node(node) => node,
        NodeOrToken::Token(token) => token.parent()?,
    };
    first.ancestors().find(|it| path::is_top(*it) && it.text_range().start() == offset)
}

/// The path an item touched is named by: in the base file, or as the edits left it when they
/// added it; empty for the file's own module.
pub fn path_at(file: &Working, at: &At) -> String {
    match at {
        At::Item(offset) => path::of(item_at(file.base(), *offset).expect("a touched item starts there")),
        At::File => String::new(),
        At::Added(path) => path.clone(),
    }
}

/// Where a top item of the file as edited so far was in the base file.
pub fn base_of(file: &Working, item: Node<'_>) -> Result<At, String> {
    file.to_base(item.text_range().start()).map(At::Item).ok_or_else(|| {
        format!(
            "{}: `{}` was added by an earlier edit of this request: edit it in the edit that adds it",
            file.path,
            path::of(item)
        )
    })
}

/// The scope a top item is in: its module, impl or trait, or the file.
fn scope_at(file: &Working, item: Node<'_>) -> Result<At, String> {
    match path::scope_of(item) {
        Some(scope) => base_of(file, scope),
        None => Ok(At::File),
    }
}

/// The module, impl or trait `into` names, `None` for the file.
fn container<'f>(file: &'f Working, into: Option<&str>) -> Result<Option<Node<'f>>, String> {
    let Some(into) = into else { return Ok(None) };
    let node = path::find(file.root(), into).map_err(|error| format!("{}: {error}", file.path))?;
    let node = expect(node, &[Kind::Mod, Kind::Impl, Kind::Trait], into)?;
    if node.kind() == SyntaxKind::MODULE
        && node.child_where(|it| it == SyntaxKind::ITEM_LIST).is_none()
    {
        return Err(format!(
            "{}: `mod {into};` is in a file of its own: name that file and no `into`",
            file.path
        ));
    }
    Ok(Some(node))
}

/// The items a scope (the file, a module, an impl or a trait) holds, in order, uses and
/// macro calls among them.
fn items_of<'t>(scope: Node<'t>) -> impl Iterator<Item = Node<'t>> {
    let list = match scope.kind() {
        SyntaxKind::SOURCE_FILE => Some(scope),
        SyntaxKind::MODULE => scope.child_where(|it| it == SyntaxKind::ITEM_LIST),
        _ => scope.child_where(|it| it == SyntaxKind::ASSOC_ITEM_LIST),
    };
    list.into_iter()
        .flat_map(Node::children)
        .filter(|it| ast::Item::can_cast(it.kind()) || ast::AssocItem::can_cast(it.kind()))
}

/// The braces (or the file's ends) of a scope's list.
fn list_bounds(scope: Node<'_>, end: usize) -> Option<(TextSize, TextSize)> {
    if scope.kind() == SyntaxKind::SOURCE_FILE {
        let start = scope
            .children_with_tokens()
            .filter(|it| it.kind() == SyntaxKind::SHEBANG || it.kind() == SyntaxKind::FRONTMATTER)
            .map(|it| it.text_range().end())
            .last()
            .unwrap_or_default();
        return Some((start, TextSize::try_from(end).expect("a file under 4 GiB")));
    }
    let list = match scope.kind() {
        SyntaxKind::MODULE => scope.child_where(|it| it == SyntaxKind::ITEM_LIST)?,
        _ => scope.child_where(|it| it == SyntaxKind::ASSOC_ITEM_LIST)?,
    };
    let open = list.child_token(SyntaxKind::L_CURLY)?;
    let close = list.child_token(SyntaxKind::R_CURLY)?;
    Some((open.text_range().end(), close.text_range().start()))
}

/// The first token of an item that is neither an attribute, a comment nor whitespace: where its
/// signature begins.
fn first_own_token(node: Node<'_>) -> Option<TextSize> {
    node.children_with_tokens()
        .find(|it| it.kind() != SyntaxKind::ATTR && !it.kind().is_trivia())
        .map(|it| it.text_range().start())
}

/// `end` moved back over whitespace, never before `start`.
fn trim_back(text: &str, start: TextSize, end: TextSize) -> TextSize {
    let slice = &text[usize::from(start)..usize::from(end)];
    start + TextSize::of(slice.trim_end())
}

/// The comma right after `node` among its siblings, past trivia.
fn comma_after(node: Node<'_>) -> Option<Token<'_>> {
    let mut at = node.next_sibling_or_token();
    while let Some(element) = at {
        match element {
            NodeOrToken::Token(token) if token.kind().is_trivia() => at = token.next_sibling_or_token(),
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::COMMA => return Some(token),
            _ => return None,
        }
    }
    None
}

/// The indentation of the line `at` is on, when only whitespace comes before `at` on it.
fn indent_at(text: &str, at: usize) -> &str {
    let start = text[..at].rfind('\n').map_or(0, |it| it + 1);
    let lead = &text[start..at];
    if lead.chars().all(char::is_whitespace) { lead } else { "" }
}

/// Past the rest of `at`'s line when it holds only whitespace and a line comment and ends
/// before `limit`: where a new line goes after what ends at `at`.
fn line_end(text: &str, at: usize, limit: usize) -> usize {
    let rest = &text[at..limit];
    match rest.find('\n') {
        Some(newline) => {
            let line = rest[..newline].trim_start();
            if line.is_empty() || line.starts_with("//") { at + newline } else { at }
        }
        None => at,
    }
}

/// Whether a field's text is a named field (`x: T`), not a tuple field's type.
fn is_named(field: &str) -> bool {
    let bytes = field.as_bytes();
    bytes.iter().enumerate().any(|(at, &byte)| {
        byte == b':'
            && bytes.get(at + 1) != Some(&b':')
            && (at == 0 || bytes[at - 1] != b':')
    })
}

fn size(at: usize) -> TextSize {
    TextSize::try_from(at).expect("a file under 4 GiB")
}

/// The splice adding `field` last to `node`'s fields, in the list's own style: one per line
/// when the list spans lines, a trailing comma when the last field has one.
fn add_field(text: &str, node: Node<'_>, field: &str) -> Result<(TextRange, String), String> {
    let field = field.trim();
    let named = is_named(field);
    let list = node.child_where(|it| it == SyntaxKind::RECORD_FIELD_LIST || it == SyntaxKind::TUPLE_FIELD_LIST);
    let Some(list) = list else {
        let (open, close) = if named { (" { ", " }") } else { ("(", ")") };
        let insert = format!("{open}{field}{close}");
        return Ok(match ast::Struct::cast(node).and_then(|it| it.semicolon_token()) {
            Some(semicolon) if named => (semicolon.text_range(), insert),
            Some(semicolon) => (TextRange::empty(semicolon.text_range().start()), insert),
            None => {
                let name = node
                    .child_where(|it| it == SyntaxKind::NAME)
                    .ok_or_else(|| "a variant with no name".to_owned())?;
                let after = node
                    .child_where(|it| it == SyntaxKind::GENERIC_PARAM_LIST)
                    .unwrap_or(name)
                    .text_range()
                    .end();
                (TextRange::empty(after), insert)
            }
        });
    };
    let record = list.kind() == SyntaxKind::RECORD_FIELD_LIST;
    if record != named {
        return Err(format!(
            "`{field}` is a {} field and `{}` has {} fields",
            if named { "named" } else { "tuple" },
            path::of(node),
            if record { "named" } else { "tuple" }
        ));
    }
    let (open, close) = if record {
        (SyntaxKind::L_CURLY, SyntaxKind::R_CURLY)
    } else {
        (SyntaxKind::L_PAREN, SyntaxKind::R_PAREN)
    };
    let open = list.child_token(open).ok_or("a field list with no opening bracket")?;
    let close = list.child_token(close).ok_or("a field list with no closing bracket")?;
    let last = path::members(node).last();
    add_entry(text, last, open.text_range().end(), close.text_range().start(), field, record)
}

/// The splice adding `entry` after `last`, the last of the entries that lie between `open` and
/// `close`.
fn add_entry(
    text: &str,
    last: Option<Node<'_>>,
    open: TextSize,
    close: TextSize,
    entry: &str,
    spaced: bool,
) -> Result<(TextRange, String), String> {
    let Some(last) = last else {
        let insert = if spaced { format!(" {entry} ") } else { entry.to_owned() };
        return Ok((TextRange::new(open, close), insert));
    };
    let multiline = text[usize::from(open)..usize::from(close)].contains('\n');
    let comma = comma_after(last);
    let anchor = comma.as_ref().map_or(last.text_range().end(), |it| it.text_range().end());
    if !multiline {
        let insert = match comma {
            Some(_) => format!(" {entry},"),
            None => format!(", {entry}"),
        };
        return Ok((TextRange::empty(anchor), insert));
    }
    let indent = indent_at(text, usize::from(last.text_range().start()));
    let at = line_end(text, usize::from(anchor), usize::from(close));
    Ok(match comma {
        Some(_) => (TextRange::empty(size(at)), format!("\n{indent}{entry},")),
        None => {
            let kept = &text[usize::from(anchor)..at];
            (TextRange::new(anchor, size(at)), format!(",{kept}\n{indent}{entry}"))
        }
    })
}

/// The range that removes a field or variant with its comma, keeping the list's style.
fn remove_entry(text: &str, entry: Node<'_>) -> TextRange {
    let list = entry.parent().expect("a field or variant is in a list");
    let kind = entry.kind();
    let previous = std::iter::successors(entry.prev_sibling(), |it| it.prev_sibling()).find(|it| it.kind() == kind);
    let next = std::iter::successors(entry.next_sibling(), |it| it.next_sibling()).find(|it| it.kind() == kind);
    let range = entry.text_range();
    if let Some(next) = next {
        return TextRange::new(range.start(), next.text_range().start());
    }
    let Some(previous) = previous else {
        let open = list.first_token().expect("a list opens").text_range().end();
        let close = list.last_token().expect("a list closes").text_range().start();
        return TextRange::new(open, close);
    };
    match comma_after(entry) {
        Some(own) => {
            let start = comma_after(previous).expect("a list's entries before the last end in commas");
            let stop = usize::from(own.text_range().end());
            let close = usize::from(list.text_range().end());
            TextRange::new(start.text_range().end(), size(line_end(text, stop, close)))
        }
        None => TextRange::new(previous.text_range().end(), range.end()),
    }
}

/// `range` widened to its whole lines when nothing else is on them, and one blank line before
/// it when there is one: what deleting an item leaves tidy.
fn remove_line(text: &str, end: usize, range: TextRange) -> TextRange {
    let start = usize::from(range.start());
    let stop = usize::from(range.end());
    let line_start = text[..start].rfind('\n').map_or(0, |it| it + 1);
    if !text[line_start..start].chars().all(char::is_whitespace) {
        return range;
    }
    let rest = &text[stop..end];
    let Some(newline) = rest.find('\n') else { return range };
    if !rest[..newline].trim().is_empty() {
        return range;
    }
    let mut from = line_start;
    let mut to = stop + newline + 1;
    if text[..line_start].ends_with("\n\n") {
        from -= 1;
    } else if (line_start == 0 || text[..line_start].trim_end().ends_with('{'))
        && text[to..end].starts_with('\n')
    {
        to += 1;
    }
    TextRange::new(size(from), size(to))
}

/// The splice adding `item` after the last item of `scope`.
fn add_item(text: &str, end: usize, scope: Node<'_>, item: &str) -> Result<(TextRange, String), String> {
    let item = item.trim();
    if item.is_empty() {
        return Err("the item to add is empty".to_owned());
    }
    let (open, close) = list_bounds(scope, end).ok_or("a scope with no item list")?;
    let file = scope.kind() == SyntaxKind::SOURCE_FILE;
    match items_of(scope).last() {
        Some(last) => {
            let indent = indent_at(text, usize::from(last.text_range().start()));
            let at = line_end(text, usize::from(last.text_range().end()), usize::from(close));
            Ok((TextRange::empty(size(at)), format!("\n\n{indent}{}", indented(item, indent))))
        }
        None if file => {
            let before = &text[usize::from(open)..usize::from(close)];
            let lead = if before.is_empty() || before.ends_with('\n') { "" } else { "\n" };
            Ok((TextRange::empty(close), format!("{lead}{item}\n")))
        }
        None => {
            let outer = indent_at(text, usize::from(scope.text_range().start()));
            let inner = format!("{outer}    ");
            Ok((TextRange::new(open, close), format!("\n{inner}{}\n{outer}", indented(item, &inner))))
        }
    }
}

/// The splice adding `use used;` after the scope's last use, or before its first item.
pub(crate) fn add_use(text: &str, end: usize, scope: Node<'_>, used: &str) -> Result<(TextRange, String), String> {
    let line = format!("use {used};");
    if let Some(last) = items_of(scope).filter(|it| it.kind() == SyntaxKind::USE).last() {
        let indent = indent_at(text, usize::from(last.text_range().start()));
        let (_, close) = list_bounds(scope, end).ok_or("a scope with no item list")?;
        let at = line_end(text, usize::from(last.text_range().end()), usize::from(close));
        return Ok((TextRange::empty(size(at)), format!("\n{indent}{line}")));
    }
    match items_of(scope).next() {
        Some(first) => {
            let indent = indent_at(text, usize::from(first.text_range().start()));
            Ok((TextRange::empty(first.text_range().start()), format!("{line}\n\n{indent}")))
        }
        None => add_item(text, end, scope, &line),
    }
}

/// `item`'s lines after the first indented by `indent`.
fn indented(item: &str, indent: &str) -> String {
    if indent.is_empty() {
        return item.to_owned();
    }
    let mut out = String::with_capacity(item.len() + indent.len() * 4);
    for (at, line) in item.split('\n').enumerate() {
        if at > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(indent);
            }
        }
        out.push_str(line);
    }
    out
}

/// The top items wholly inside `inserted`, as the edit left the file: found from the node
/// covering it up, so no body is parsed.
fn added(file: &Working, index: u32, inserted: TextRange, out: &mut Vec<Touch>) {
    let start = match file.root().covering_element(inserted) {
        NodeOrToken::Node(node) => node,
        NodeOrToken::Token(token) => token.parent().expect("a token has a parent"),
    };
    for node in start.ancestors() {
        let mut items = path::members(node)
            .filter(|it| path::is_top(*it) && inserted.contains_range(it.text_range()))
            .peekable();
        if items.peek().is_some() {
            out.extend(items.map(|it| Touch { file: index, at: At::Added(path::of(it)), how: How::Added }));
            return;
        }
    }
}
