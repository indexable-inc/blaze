//! The hooks of an identity's closure held to the reviewed list (C1 of the review), on blaze's
//! own tree: Rust read as tokens, and only the files a crate is compiled from.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{CODEGEN, FRONT_END, Hook, Identity, RECORD};

/// A token of Rust, as far as the hooks need: comments are dropped, and a lifetime or a
/// character is no token.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Token {
    Ident(String),
    /// A string literal's contents, as written.
    Str(String),
    /// `::`.
    Path,
    /// `->`.
    Arrow,
    Punct(char),
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The tokens of `text`.
pub(super) fn tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    let n = chars.len();
    let ahead =
        |at: usize, s: &str| s.chars().enumerate().all(|(i, c)| chars.get(at + i) == Some(&c));
    while at < n {
        let c = chars[at];
        if c.is_whitespace() {
            at += 1;
        } else if ahead(at, "//") {
            while at < n && chars[at] != '\n' {
                at += 1;
            }
        } else if ahead(at, "/*") {
            let mut depth = 0;
            while at < n {
                if ahead(at, "/*") {
                    depth += 1;
                    at += 2;
                } else if ahead(at, "*/") {
                    depth -= 1;
                    at += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    at += 1;
                }
            }
        } else if c == '"' {
            let start = at + 1;
            at = start;
            while at < n && chars[at] != '"' {
                at += if chars[at] == '\\' { 2 } else { 1 };
            }
            out.push(Token::Str(chars[start..at.min(n)].iter().collect()));
            at += 1;
        } else if c == '\'' {
            // A character (`'a'`, `'\n'`, `'\u{1F600}'`) or a lifetime (`'a`), neither a token.
            if chars.get(at + 1) == Some(&'\\') {
                // Past the escaped character, which may be `'` itself.
                at += 3;
                while at < n && chars[at] != '\'' {
                    at += 1;
                }
                at += 1;
            } else if chars.get(at + 2) == Some(&'\'') {
                at += 3;
            } else {
                at += 1;
                while at < n && ident_char(chars[at]) {
                    at += 1;
                }
            }
        } else if ident_char(c) {
            let start = at;
            while at < n && ident_char(chars[at]) {
                at += 1;
            }
            let word: String = chars[start..at].iter().collect();
            // A raw string (`r"..."`, `r#"..."#`, `br#"..."#`), or a prefixed string or byte.
            let raw = matches!(word.as_str(), "r" | "br" | "cr")
                && matches!(chars.get(at), Some('"' | '#'));
            if raw {
                let mut hashes = 0;
                while chars.get(at) == Some(&'#') {
                    hashes += 1;
                    at += 1;
                }
                if chars.get(at) == Some(&'"') {
                    let start = at + 1;
                    let mut end = start;
                    while end < n
                        && !(chars[end] == '"'
                            && (1..=hashes).all(|i| chars.get(end + i) == Some(&'#')))
                    {
                        end += 1;
                    }
                    out.push(Token::Str(chars[start..end.min(n)].iter().collect()));
                    at = end + 1 + hashes;
                    continue;
                }
                out.push(Token::Ident(word));
                continue;
            }
            if matches!(word.as_str(), "b" | "c") && matches!(chars.get(at), Some('"' | '\'')) {
                continue;
            }
            out.push(Token::Ident(word));
        } else if ahead(at, "::") {
            out.push(Token::Path);
            at += 2;
        } else if ahead(at, "->") {
            out.push(Token::Arrow);
            at += 2;
        } else {
            out.push(Token::Punct(c));
            at += 1;
        }
    }
    out
}

fn is(token: Option<&Token>, word: &str) -> bool {
    matches!(token, Some(Token::Ident(it)) if it == word)
}

/// `tokens` without the body of each inline module under `#[cfg(test)]` (or
/// `#[cfg(all(test, ...))]`): code a crate's tests are built from, never the crate, as
/// [`compiled`] leaves out such a module's file.
pub(super) fn outside_tests(tokens: &[Token]) -> Vec<Token> {
    let attr_end = |at: usize| -> Option<usize> {
        if !punct(tokens.get(at), '#') || !punct(tokens.get(at + 1), '[') {
            return None;
        }
        let mut nested = 0i32;
        let mut end = at + 1;
        while end < tokens.len() {
            match tokens[end] {
                Token::Punct('[') => nested += 1,
                Token::Punct(']') => nested -= 1,
                _ => {}
            }
            end += 1;
            if nested == 0 {
                return Some(end);
            }
        }
        None
    };
    let is_test = |at: usize| {
        let word = |i: usize, w: &str| matches!(tokens.get(at + i), Some(Token::Ident(x)) if x == w);
        word(2, "cfg")
            && punct(tokens.get(at + 3), '(')
            && ((word(4, "test") && punct(tokens.get(at + 5), ')'))
                || (word(4, "all") && punct(tokens.get(at + 5), '(') && word(6, "test")))
    };
    let mut out = Vec::with_capacity(tokens.len());
    let mut at = 0;
    while at < tokens.len() {
        if let Some(end) = attr_end(at)
            && is_test(at)
        {
            // Past this and any further attributes and a visibility, to `mod name {`.
            let mut next = end;
            while let Some(end) = attr_end(next) {
                next = end;
            }
            if matches!(tokens.get(next), Some(Token::Ident(it)) if it == "pub") {
                next += 1;
                if punct(tokens.get(next), '(') {
                    while next < tokens.len() && !punct(tokens.get(next), ')') {
                        next += 1;
                    }
                    next += 1;
                }
            }
            if matches!(tokens.get(next), Some(Token::Ident(it)) if it == "mod")
                && matches!(tokens.get(next + 1), Some(Token::Ident(_)))
                && punct(tokens.get(next + 2), '{')
            {
                let mut nested = 0i32;
                let mut end = next + 2;
                while end < tokens.len() {
                    match tokens[end] {
                        Token::Punct('{') => nested += 1,
                        Token::Punct('}') => nested -= 1,
                        _ => {}
                    }
                    end += 1;
                    if nested == 0 {
                        break;
                    }
                }
                at = end;
                continue;
            }
        }
        out.push(tokens[at].clone());
        at += 1;
    }
    out
}

#[test]
fn a_test_module_is_outside_and_the_rest_is_kept() {
    let text = "use a::B;\n#[cfg(test)]\nmod tests { use c::D; fn f() { {} } }\n\
                #[cfg(all(test, unix))] pub(crate) mod more { e::F }\nfn g() { h::I }";
    let kept = outside_tests(&tokens(text));
    let words: Vec<&str> = kept
        .iter()
        .filter_map(|it| match it {
            Token::Ident(it) => Some(it.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(words, ["use", "a", "B", "fn", "g", "h", "I"]);
}

fn punct(token: Option<&Token>, c: char) -> bool {
    token == Some(&Token::Punct(c))
}

/// The index past the `<...>` that starts at `at`, balanced.
fn past_generics(tokens: &[Token], mut at: usize) -> usize {
    let mut depth = 0i32;
    while at < tokens.len() {
        match tokens[at] {
            Token::Punct('<') => depth += 1,
            Token::Punct('>') => depth -= 1,
            _ => {}
        }
        at += 1;
        if depth == 0 {
            break;
        }
    }
    at
}

/// The path that starts at `at` (`a::b::C<T>`): its last segment and the index past it.
fn path_at(tokens: &[Token], mut at: usize) -> Option<(String, usize)> {
    if tokens.get(at) == Some(&Token::Path) {
        at += 1;
    }
    let mut last = None;
    loop {
        let Some(Token::Ident(segment)) = tokens.get(at) else { return last.map(|it| (it, at)) };
        last = Some(segment.clone());
        at += 1;
        if punct(tokens.get(at), '<') {
            at = past_generics(tokens, at);
        }
        if tokens.get(at) == Some(&Token::Path) {
            at += 1;
        } else {
            return last.map(|it| (it, at));
        }
    }
}

/// What a file's tokens say of traits: those it defines, those it names `dyn`, and those it
/// implements, each with the line of its `impl`.
#[derive(Default, Debug)]
pub(super) struct Traits {
    pub(super) defined: BTreeSet<String>,
    pub(super) dyns: BTreeSet<String>,
    pub(super) implemented: Vec<String>,
}

pub(super) fn traits(tokens: &[Token]) -> Traits {
    let mut out = Traits::default();
    for (at, token) in tokens.iter().enumerate() {
        let Token::Ident(word) = token else { continue };
        match word.as_str() {
            "trait" => {
                if let Some(Token::Ident(name)) = tokens.get(at + 1) {
                    out.defined.insert(name.clone());
                }
            }
            "dyn" => {
                let mut next = at + 1;
                if is(tokens.get(next), "for") && punct(tokens.get(next + 1), '<') {
                    next = past_generics(tokens, next + 1);
                }
                if let Some((name, _)) = path_at(tokens, next) {
                    out.dyns.insert(name);
                }
            }
            "impl" => {
                let mut next = at + 1;
                if punct(tokens.get(next), '<') {
                    next = past_generics(tokens, next);
                }
                if punct(tokens.get(next), '!') {
                    next += 1;
                }
                if is(tokens.get(next), "dyn") {
                    continue;
                }
                if let Some((name, past)) = path_at(tokens, next)
                    && is(tokens.get(past), "for")
                {
                    out.implemented.push(name);
                }
            }
            _ => {}
        }
    }
    out
}

/// The files of the crate whose root is `root`: it and every file a `mod name;` reaches from it
/// (`#[path]` too, and `include!`), but a module under `#[cfg(test)]`. A module declared and not
/// there is an error naming it.
pub(super) fn compiled(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    // A file, and the directory its own `mod name;` items are in.
    let mut stack = vec![(root.to_path_buf(), root.parent().unwrap().to_path_buf())];
    while let Some((file, base)) = stack.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        out.push(file.clone());
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|why| panic!("{}: {why}", file.display()));
        let tokens = tokens(&text);
        let dir = file.parent().unwrap().to_path_buf();
        // Inline modules open at each brace depth, by name.
        let mut inline: Vec<(String, usize)> = Vec::new();
        let mut depth = 0usize;
        let mut attrs: Vec<String> = Vec::new();
        let mut at = 0;
        while at < tokens.len() {
            match &tokens[at] {
                Token::Punct('#') => {
                    let mut end = at + 1;
                    if punct(tokens.get(end), '!') {
                        end += 1;
                    }
                    if punct(tokens.get(end), '[') {
                        let start = end;
                        let mut nested = 0i32;
                        while end < tokens.len() {
                            match tokens[end] {
                                Token::Punct('[') => nested += 1,
                                Token::Punct(']') => nested -= 1,
                                _ => {}
                            }
                            end += 1;
                            if nested == 0 {
                                break;
                            }
                        }
                        let attr: String = tokens[start..end]
                            .iter()
                            .map(|it| match it {
                                Token::Ident(it) => it.clone(),
                                Token::Str(it) => format!("\"{it}\""),
                                Token::Path => "::".to_owned(),
                                Token::Arrow => "->".to_owned(),
                                Token::Punct(c) => c.to_string(),
                            })
                            .collect();
                        attrs.push(attr);
                        at = end;
                        continue;
                    }
                }
                Token::Punct('{') => {
                    depth += 1;
                    attrs.clear();
                }
                Token::Punct('}') => {
                    depth = depth.saturating_sub(1);
                    if inline.last().is_some_and(|(_, at)| *at == depth) {
                        inline.pop();
                    }
                    attrs.clear();
                }
                Token::Punct(';') => attrs.clear(),
                Token::Ident(word)
                    if word == "include"
                        && punct(tokens.get(at + 1), '!')
                        && punct(tokens.get(at + 2), '(') =>
                {
                    if let Some(Token::Str(path)) = tokens.get(at + 3) {
                        let included = dir.join(path);
                        if included.is_file() {
                            stack
                                .push((included.clone(), included.parent().unwrap().to_path_buf()));
                        }
                    }
                }
                Token::Ident(word) if word == "mod" => {
                    let test = attrs
                        .iter()
                        .any(|it| it == "[cfg(test)]" || it.starts_with("[cfg(all(test,"));
                    let Some(Token::Ident(name)) = tokens.get(at + 1) else {
                        at += 1;
                        continue;
                    };
                    let path = attrs.iter().find_map(|it| {
                        it.strip_prefix("[path=\"")?.strip_suffix("\"]").map(str::to_owned)
                    });
                    match tokens.get(at + 2) {
                        Some(Token::Punct(';')) if !test => {
                            let mut under = base.clone();
                            for (name, _) in &inline {
                                under.push(name);
                            }
                            let candidates = match &path {
                                Some(path) if inline.is_empty() => vec![dir.join(path)],
                                Some(path) => vec![under.join(path)],
                                None => vec![
                                    under.join(format!("{name}.rs")),
                                    under.join(name).join("mod.rs"),
                                ],
                            };
                            let found = candidates.iter().find(|it| it.is_file()).unwrap_or_else(|| {
                                panic!("{} declares `mod {name};`, and none of {candidates:?} is there", file.display())
                            });
                            let own = found.parent().unwrap().join(found.file_stem().unwrap());
                            let base = if found.file_name().is_some_and(|it| it == "mod.rs")
                                || path.is_some()
                            {
                                found.parent().unwrap().to_path_buf()
                            } else {
                                own
                            };
                            stack.push((found.clone(), base));
                        }
                        Some(Token::Punct('{')) => {
                            if test {
                                // Skip the test module's body.
                                let mut nested = 0i32;
                                let mut end = at + 2;
                                while end < tokens.len() {
                                    match tokens[end] {
                                        Token::Punct('{') => nested += 1,
                                        Token::Punct('}') => nested -= 1,
                                        _ => {}
                                    }
                                    end += 1;
                                    if nested == 0 {
                                        break;
                                    }
                                }
                                attrs.clear();
                                at = end;
                                continue;
                            }
                            inline.push((name.clone(), depth));
                        }
                        _ => {}
                    }
                    attrs.clear();
                }
                _ => {}
            }
            at += 1;
        }
    }
    out
}

/// The roots of what the package at `dir` puts in a program: its library, and its binaries when
/// it is the program itself.
pub(super) fn roots(dir: &Path, program: bool) -> Vec<PathBuf> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    let manifest: toml::Table = text.parse().unwrap();
    let mut out = Vec::new();
    let lib = manifest
        .get("lib")
        .and_then(|it| it.get("path"))
        .and_then(toml::Value::as_str)
        .unwrap_or("src/lib.rs");
    if dir.join(lib).is_file() {
        out.push(dir.join(lib));
    }
    if program {
        for bin in manifest.get("bin").and_then(toml::Value::as_array).into_iter().flatten() {
            if let Some(path) = bin.get("path").and_then(toml::Value::as_str) {
                out.push(dir.join(path));
            }
        }
        if dir.join("src/main.rs").is_file() {
            out.push(dir.join("src/main.rs"));
        }
    }
    out
}

/// What the files a package puts in the program say of traits.
fn package_traits(dir: &Path, program: bool) -> Vec<(PathBuf, Traits)> {
    let mut files: Vec<PathBuf> = roots(dir, program).iter().flat_map(|it| compiled(it)).collect();
    files.sort();
    files.dedup();
    files
        .into_iter()
        .map(|file| {
            let text = std::fs::read_to_string(&file).unwrap();
            let traits = traits(&tokens(&text));
            (file, traits)
        })
        .collect()
}

/// For blaze-engine's program and each of `identities`: every implementation, in a package of the
/// checkout the program links and outside the closure, of a hook trait (one the identity lists,
/// or one a package of the closure defines and names `dyn`, implemented by a package built from
/// it) is reviewed; and every reviewed one of a linked package is still there.
fn unreviewed(identities: &[&Identity]) -> Vec<String> {
    let engine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-engine");
    let workspace = crate::Workspace::read(&engine).unwrap();
    let linked = workspace.linked_names().unwrap();
    let checkout: BTreeMap<&str, &Path> = workspace.checkout().into_iter().collect();
    // Every package the program links is scanned once; each closure is of linked packages.
    let scanned: BTreeMap<&str, Vec<(PathBuf, Traits)>> = checkout
        .iter()
        .filter(|&(&name, _)| linked.contains(name))
        .map(|(&name, &dir)| (name, package_traits(dir, name == workspace.program)))
        .collect();
    let mut problems = Vec::new();
    for identity in identities {
        let closure = workspace.closure_names_of(identity).unwrap();
        // Each hook by name, with the packages of the closure that define it (none: listed).
        let mut hooks: BTreeMap<String, BTreeSet<String>> =
            identity.hooks.iter().map(|it| ((*it).to_owned(), BTreeSet::new())).collect();
        let mut defined: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut dyns = BTreeSet::new();
        for package in closure.iter().filter(|it| checkout.contains_key(it.as_str())) {
            for (_, traits) in &scanned[package.as_str()] {
                for name in &traits.defined {
                    defined.entry(name.clone()).or_default().insert(package.clone());
                }
                dyns.extend(traits.dyns.iter().cloned());
            }
        }
        for (name, packages) in defined {
            if dyns.contains(&name) {
                hooks.entry(name).or_default().extend(packages);
            }
        }
        let mut found = BTreeSet::new();
        for package in
            linked.iter().filter(|it| checkout.contains_key(it.as_str()) && !closure.contains(*it))
        {
            let built_from = workspace.built_from_names(package).unwrap();
            for (file, traits) in &scanned[package.as_str()] {
                for name in &traits.implemented {
                    let Some(defined_in) = hooks.get(name) else { continue };
                    // A trait of the same name it is not built from is another trait.
                    if !defined_in.is_empty()
                        && !defined_in.iter().any(|it| built_from.contains(it) && it != package)
                    {
                        continue;
                    }
                    found.insert((package.clone(), name.clone()));
                    if !identity
                        .reviewed
                        .iter()
                        .any(|it| it.package == package.as_str() && it.hook == name.as_str())
                    {
                        problems.push(format!(
                            "{}: {package} implements `{name}`, a hook of {}: add its package to its roots, or review \
                             it into its reviewed hooks",
                            file.display(),
                            identity.what
                        ));
                    }
                }
            }
        }
        let stale: Vec<&Hook> = identity
            .reviewed
            .iter()
            .filter(|it| {
                linked.contains(it.package)
                    && !found.contains(&(it.package.to_owned(), it.hook.to_owned()))
            })
            .collect();
        for hook in stale {
            problems.push(format!(
                "{}: reviewed hook no longer implemented past its closure: {hook:?}",
                identity.what
            ));
        }
    }
    problems
}

/// C1 of the review, held on blaze's own tree for the code generator's identity, the front
/// end's and the record's: code past an identity's closure (a cut package among it) that its
/// closure runs through a hook is a root of it (so an input) or reviewed.
#[test]
fn every_hook_implemented_past_the_closure_is_reviewed() {
    let problems = unreviewed(&[&CODEGEN, &FRONT_END, &RECORD]);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn an_impl_names_its_trait_and_a_fixture_string_names_none() {
    let text = r##"
        impl BodyFacts for Bodies {}
        impl blaze_codegen::driver::Lookup for Reading<'_> {}
        impl<'db, T: Into<U>> Prepare<T> for Backends<'db> {}
        unsafe impl Send for X {}
        impl Bodies { fn f() -> impl Iterator<Item = u8> { for x in y {} } }
        impl dyn Shown { }
        const FIXTURE: &str = "impl Fake for X {} trait Faked {}";
        const RAW: &str = r#"impl AlsoFake for X {}"#;
        // impl Commented for X {}
        /* impl Blocked for X { /* nested */ } */
        let c = '"'; let e = '\''; let l: &'static str = "";
        fn g(f: &dyn for<'a> Fn(&'a u8)) {}
        pub trait Hook { fn f(&self, it: &dyn other::Hooked<'_>); }
        impl Last for Y {}
    "##;
    let traits = traits(&tokens(text));
    assert_eq!(traits.implemented, ["BodyFacts", "Lookup", "Prepare", "Send", "Last"]);
    assert_eq!(traits.defined, ["Hook".to_owned()].into_iter().collect());
    assert_eq!(
        traits.dyns,
        ["Hooked".to_owned(), "Shown".to_owned(), "Fn".to_owned()].into_iter().collect()
    );
}
