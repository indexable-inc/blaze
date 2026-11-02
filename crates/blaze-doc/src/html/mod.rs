//! The documentation as files under `doc/`, laid out as rustdoc lays them out: a page per item
//! (`serde/de/trait.Deserialize.html`, a module's `index.html`), `all.html` and
//! `sidebar-items.js` per crate and module, a source page per file under `src/`, and a search
//! index per crate that the search box loads.
//!
//! The stylesheet and script are this crate's own (`static/`); the page structure, class names
//! and ids follow rustdoc's so links into rustdoc's docs and readers' habits carry over.

use std::{
    fmt::Write as _,
    io,
    path::Path,
};

use blaze_hash::{HashMap, HashSet};

use crate::{
    markdown::{self, Context, Ids},
    model::{Code, CrateDoc, Doc, ImplDoc, Item, Kind, Member, Model, Notes, Piece, Span, Target},
};

const CSS: &str = include_str!("../../static/blaze.css");
const JS: &str = include_str!("../../static/blaze.js");

/// The crates doc.rust-lang.org documents, which links to `std` items point at.
const SYSROOT: [&str; 5] = ["std", "core", "alloc", "proc_macro", "test"];

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// What was written.
#[derive(Debug, Default)]
pub struct Written {
    pub pages: usize,
    pub source_pages: usize,
}

/// One file of the documentation: its path under `doc/`, `/`-separated, and its bytes.
#[derive(Debug)]
pub struct Page {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Every file of the documentation but `crates.js` ([`crates_list`]), which also names the
/// crates earlier runs documented in the same directory.
#[derive(Debug, Default)]
pub struct Pages {
    pub files: Vec<Page>,
    pub written: Written,
}

struct Statics {
    css: String,
    js: String,
}

fn hash(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = blaze_hash::FixedHasher::default();
    text.hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

/// Every page of `model`, made in parallel: item pages, each module's `sidebar-items.js`,
/// source pages, each crate's `all.html` and search index, and the stylesheet and script.
pub fn pages(model: &Model) -> Pages {
    use blaze_graph::par;
    let statics = Statics {
        css: format!("static.files/blaze-{}.css", hash(CSS)),
        js: format!("static.files/blaze-{}.js", hash(JS)),
    };
    let names: HashSet<&str> = model.crates.iter().map(|it| it.name.as_str()).collect();
    let page = |path: String, text: String| Page { path, bytes: text.into_bytes() };
    let mut files = vec![page(statics.css.clone(), CSS.to_owned()), page(statics.js.clone(), JS.to_owned())];

    let jobs: Vec<(usize, usize)> = model
        .crates
        .iter()
        .enumerate()
        .flat_map(|(k, krate)| (0..krate.items.len()).map(move |i| (k, i)))
        .collect();
    let items: Vec<Vec<Page>> = par::map(&jobs, |&(k, i)| {
        let krate = &model.crates[k];
        let item = &krate.items[i];
        let writer = Writer::new(model, &names, &statics, k, page_dir(krate, item));
        let mut out = vec![page(page_path(krate, item), writer.item_page(i))];
        if item.kind == Kind::Module {
            let path = format!("{}/sidebar-items.js", module_dir(krate, item));
            out.push(page(path, sidebar_items(krate, item)));
        }
        out
    });
    let pages = items.len();
    files.extend(items.into_iter().flatten());
    let sources: Vec<(usize, usize)> = model
        .crates
        .iter()
        .enumerate()
        .flat_map(|(k, krate)| (0..krate.sources.len()).map(move |f| (k, f)))
        .collect();
    let source_files: Vec<Page> = par::map(&sources, |&(k, f)| {
        let krate = &model.crates[k];
        let file = &krate.sources[f];
        let mut dir = vec!["src".to_owned(), krate.name.clone()];
        dir.extend(file.path.split('/').map(str::to_owned));
        dir.pop();
        let writer = Writer::new(model, &names, &statics, k, dir);
        page(format!("src/{}/{}.html", krate.name, file.path), writer.source_page(f))
    });
    let source_pages = source_files.len();
    files.extend(source_files);
    let per_crate: Vec<[Page; 2]> = par::map(par::enumerate(&model.crates), |(k, krate)| {
        let writer = Writer::new(model, &names, &statics, k, vec![krate.name.clone()]);
        [
            page(format!("{}/all.html", krate.name), writer.all_items()),
            page(format!("search-index/{}.js", krate.name), search_index(krate)),
        ]
    });
    files.extend(per_crate.into_iter().flatten());
    Pages { files, written: Written { pages: pages + model.crates.len(), source_pages } }
}

/// Writes every page of `model` under `out` (`target/doc`), and `crates.js`.
pub fn write(model: &Model, out: &Path) -> io::Result<Written> {
    let Pages { files, written } = pages(model);
    for file in &files {
        let path = out.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &file.bytes)?;
    }
    let earlier = std::fs::read_to_string(out.join("crates.js")).ok();
    let names: Vec<&str> = model.crates.iter().map(|it| it.name.as_str()).collect();
    let list = crates_list(earlier.as_deref(), |name| out.join(name).join("index.html").exists(), &names);
    std::fs::write(out.join("crates.js"), list)?;
    Ok(written)
}

/// `crates.js`: every crate of `names`, and those `earlier` (the file as it was) named that
/// still have their pages (`kept`), sorted.
pub fn crates_list(earlier: Option<&str>, kept: impl Fn(&str) -> bool, names: &[&str]) -> String {
    let mut listed: Vec<String> = earlier
        .and_then(|text| {
            let start = text.find('[')?;
            let end = text.rfind(']')?;
            serde_json::from_str::<Vec<String>>(&text[start..=end]).ok()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|name| kept(name))
        .collect();
    for name in names {
        if !listed.iter().any(|it| it == name) {
            listed.push((*name).to_owned());
        }
    }
    listed.sort();
    format!("window.BLAZE_CRATES = {};\n", serde_json::to_string(&listed).unwrap())
}

fn module_dir(krate: &CrateDoc, module: &Item) -> String {
    let mut parts = vec![krate.name.as_str()];
    parts.extend(module.modules.iter().map(String::as_str));
    if !module.name.is_empty() {
        parts.push(&module.name);
    }
    parts.join("/")
}

/// The directory a page is in, from the documentation root.
fn page_dir(krate: &CrateDoc, item: &Item) -> Vec<String> {
    let mut parts = vec![krate.name.clone()];
    parts.extend(item.modules.iter().cloned());
    if item.kind == Kind::Module && !item.name.is_empty() {
        parts.push(item.name.clone());
    }
    parts
}

fn page_path(krate: &CrateDoc, item: &Item) -> String {
    let dir = page_dir(krate, item).join("/");
    if item.kind == Kind::Module {
        format!("{dir}/index.html")
    } else {
        format!("{dir}/{}.{}.html", item.kind.short(), item.name)
    }
}

/// A module's items by kind, as rustdoc's `sidebar-items.js` lists them.
fn sidebar_items(krate: &CrateDoc, module: &Item) -> String {
    let mut by_kind: Vec<(Kind, Vec<&str>)> = vec![];
    for kind in Kind::MODULE_ORDER {
        let mut names: Vec<&str> = module
            .children
            .iter()
            .map(|&at| &krate.items[at])
            .filter(|it| it.kind == kind)
            .map(|it| it.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        if !names.is_empty() {
            by_kind.push((kind, names));
        }
    }
    let mut out = String::from("window.SIDEBAR_ITEMS = {");
    for (at, (kind, names)) in by_kind.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\":{}", kind.short(), serde_json::to_string(names).unwrap());
    }
    out.push_str("};");
    out
}

/// Every item and member of a crate for the search box: kind, name, parent path, page and a
/// line of description.
fn search_index(krate: &CrateDoc) -> String {
    let mut entries: Vec<serde_json::Value> = vec![];
    for item in &krate.items {
        if item.kind == Kind::Module && item.name.is_empty() {
            continue;
        }
        let parent = std::iter::once(krate.name.as_str())
            .chain(item.modules.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("::");
        let page = page_path(krate, item);
        entries.push(serde_json::json!([
            item.kind.short(),
            item.name,
            parent,
            page,
            markdown::plain_summary(&item.docs.markdown, 120),
        ]));
        let own = format!("{parent}::{}", item.name);
        let mut member = |m: &Member| {
            entries.push(serde_json::json!([
                m.kind.short(),
                m.name,
                own,
                format!("{page}#{}.{}", m.kind.short(), m.name),
                markdown::plain_summary(&m.docs.markdown, 120),
            ]));
        };
        item.fields.iter().for_each(&mut member);
        item.variants.iter().for_each(&mut member);
        item.trait_items.iter().for_each(&mut member);
        for imp in &item.impls.inherent {
            imp.items.iter().for_each(&mut member);
        }
    }
    format!(
        "window.blazeSearchLoad({}, {});\n",
        serde_json::to_string(&krate.name).unwrap(),
        serde_json::to_string(&entries).unwrap()
    )
}

pub(crate) struct Writer<'a> {
    model: &'a Model,
    names: &'a HashSet<&'a str>,
    statics: &'a Statics,
    k: usize,
    /// From the page's directory up to the documentation root: `../../`.
    root: String,
    ids: std::cell::RefCell<Ids>,
}

impl<'a> Writer<'a> {
    fn new(
        model: &'a Model,
        names: &'a HashSet<&'a str>,
        statics: &'a Statics,
        k: usize,
        dir: Vec<String>,
    ) -> Self {
        let root = "../".repeat(dir.len());
        let mut ids = Ids::default();
        for reserved in [
            "main-content",
            "search",
            "settings",
            "help",
            "sidebar",
            "implementations",
            "trait-implementations",
            "synthetic-implementations",
            "blanket-implementations",
            "fields",
            "variants",
            "aliased-type",
            "implementors",
            "foreign-impls",
            "required-methods",
            "provided-methods",
            "reexports",
            "modules",
            "structs",
            "enums",
            "traits",
            "functions",
            "macros",
            "constants",
            "statics",
            "types",
            "unions",
            "derives",
            "attributes",
        ] {
            ids.unique(reserved);
        }
        Writer { model, names, statics, k, root, ids: std::cell::RefCell::new(ids) }
    }

    fn krate(&self) -> &'a CrateDoc {
        &self.model.crates[self.k]
    }

    pub(crate) fn url(&self, target: &Target) -> Option<String> {
        match target {
            Target::Url(url) => Some(url.clone()),
            Target::Item(item) => {
                let anchor = item.anchor.as_ref().map(|it| format!("#{it}")).unwrap_or_default();
                // As rustdoc's `extern_location`: docs of the crate's name here first.
                if self.names.contains(item.krate.as_str()) {
                    Some(format!("{}{}{anchor}", self.root, item.page()))
                } else if let Some(remote) = &item.remote {
                    Some(format!("{remote}{}{anchor}", item.page()))
                } else if SYSROOT.contains(&item.krate.as_str()) {
                    Some(format!("https://doc.rust-lang.org/nightly/{}{anchor}", item.page()))
                } else {
                    None
                }
            }
        }
    }

    pub(crate) fn title(target: &Target) -> String {
        match target {
            Target::Url(url) => url.clone(),
            Target::Item(item) => {
                // A primitive is `std`'s but not of its paths: `primitive u8`, `method u8::max`.
                let path = if item.kind == Kind::Primitive {
                    item.name.clone()
                } else {
                    item.display_path()
                };
                if let (Some(descr), Some(anchor)) = (item.descr, &item.anchor) {
                    // `method.len` names `len`, `variant.A.field.x` names `A::x`.
                    let member = anchor
                        .split_once('.')
                        .map_or(anchor.as_str(), |(_, it)| it)
                        .replace(".field.", "::");
                    return format!("{descr} {path}::{member}");
                }
                let kind = match item.kind {
                    Kind::Module => "mod",
                    Kind::Primitive => "primitive",
                    Kind::TypeAlias => "type",
                    Kind::Function => "fn",
                    other => other.short(),
                };
                format!("{kind} {path}")
            }
        }
    }

    fn docs_html(&self, doc: &Doc, heading_offset: u32) -> String {
        let cx = Context { writer: self, heading_offset };
        markdown::render(doc, &cx, &mut self.ids.borrow_mut())
    }

    fn summary_html(&self, doc: &Doc) -> String {
        let cx = Context { writer: self, heading_offset: 1 };
        markdown::summary(doc, &cx)
    }

    /// Code as HTML: names that point somewhere as links, the item's own name as a link to
    /// `anchor` when given.
    fn code(&self, code: &Code, anchor: Option<&str>) -> String {
        let mut out = String::new();
        for piece in &code.0 {
            match piece {
                Piece::Text(text)
                | Piece::Keyword(text)
                | Piece::Plain(text)
                | Piece::Lifetime(text) => {
                    out.push_str(&escape(text));
                }
                Piece::Name(name, kind) => match anchor {
                    Some(anchor) => {
                        let _ = write!(
                            out,
                            "<a href=\"#{anchor}\" class=\"{}\">{}</a>",
                            name_class(*kind),
                            escape(name)
                        );
                    }
                    None => out.push_str(&escape(name)),
                },
                Piece::Link { text, kind, to } => match self.url(to) {
                    Some(url) => {
                        let _ = write!(
                            out,
                            "<a class=\"{}\" href=\"{}\" title=\"{}\">{}</a>",
                            kind.short(),
                            escape(&url),
                            escape(&Self::title(to)),
                            escape(text)
                        );
                    }
                    None => out.push_str(&escape(text)),
                },
                Piece::Attribute(text) => {
                    let _ = write!(out, "<span class=\"attr\">{}</span>", escape(text));
                }
                Piece::Comment(text) => {
                    let _ = write!(out, "<span class=\"comment\">{}</span>", escape(text));
                }
            }
        }
        out
    }

    fn src_link(&self, span: Option<Span>) -> Option<String> {
        let span = span?;
        let krate = self.model.crates.get(span.krate)?;
        let file = krate.sources.get(span.file)?;
        let lines = if span.lines.0 == span.lines.1 {
            format!("{}", span.lines.0)
        } else {
            format!("{}-{}", span.lines.0, span.lines.1)
        };
        Some(format!("{}src/{}/{}.html#{lines}", self.root, krate.name, file.path))
    }

    fn head(
        &self,
        title: &str,
        description: &str,
        body_class: &str,
        sidebar_js: Option<&str>,
    ) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\
             <meta name=\"generator\" content=\"blaze doc\">\
             <meta name=\"description\" content=\"{description}\">\
             <title>{title}</title>\
             <link rel=\"stylesheet\" href=\"{root}{css}\">\
             <script defer src=\"{root}{js}\"></script>\
             </head><body class=\"{body_class}\" data-root=\"{root}\" data-crate=\"{krate}\"{sidebar}>",
            description = escape(description),
            title = escape(title),
            root = self.root,
            css = self.statics.css,
            js = self.statics.js,
            krate = escape(&self.krate().name),
            sidebar = sidebar_js.map(|it| format!(" data-sidebar=\"{it}\"")).unwrap_or_default(),
        );
        out
    }

    fn topbar(&self) -> String {
        format!(
            "<form class=\"search-form\" role=\"search\" onsubmit=\"return false\">\
             <div class=\"search-wrap\"><span class=\"search-icon\" aria-hidden=\"true\"></span>\
             <input class=\"search-input\" id=\"search-input\" name=\"search\" autocomplete=\"off\" \
             spellcheck=\"false\" placeholder=\"Search {krate} and its dependencies\" type=\"search\">\
             <kbd class=\"search-key\" title=\"Press S or / to search\">/</kbd></div>\
             <a class=\"all-items\" href=\"{root}{krate}/all.html\" title=\"List of all items\">All items</a>\
             <button class=\"toggle-all\" type=\"button\" title=\"Collapse or expand all docs\">Collapse all</button>\
             </form>",
            krate = escape(&self.krate().name),
            root = self.root,
        )
    }

    fn sidebar(
        &self,
        location: &str,
        sections: &[(String, Vec<(String, String)>)],
        module: Option<(String, String)>,
    ) -> String {
        let krate = self.krate();
        let mut out = String::from("<nav class=\"sidebar\" aria-label=\"Site\">");
        let _ = write!(
            out,
            "<div class=\"sidebar-crate\"><h2><a href=\"{root}{name}/index.html\">{name}</a></h2>",
            root = self.root,
            name = escape(&krate.name)
        );
        if let Some(version) = &krate.version {
            let _ = write!(out, "<span class=\"version\">{}</span>", escape(version));
        }
        out.push_str("</div><div class=\"sidebar-elems\">");
        let _ = write!(out, "<h2 class=\"location\"><a href=\"#\">{location}</a></h2>");
        for (title, links) in sections {
            if links.is_empty() {
                continue;
            }
            // One link without text: the heading is the link, to a section that lists nothing.
            if let [(href, text)] = &links[..]
                && text.is_empty()
            {
                let _ = write!(out, "<h3><a href=\"{}\">{}</a></h3>", escape(href), escape(title));
                continue;
            }
            let _ = write!(out, "<h3>{}</h3><ul class=\"block\">", escape(title));
            for (href, text) in links {
                let _ = write!(out, "<li><a href=\"{}\">{}</a></li>", escape(href), escape(text));
            }
            out.push_str("</ul>");
        }
        if let Some((href, text)) = module {
            let _ = write!(
                out,
                "<div id=\"sidebar-module\"><h2><a href=\"{}\">{}</a></h2><div class=\"sidebar-module-items\"></div></div>",
                escape(&href),
                text
            );
        }
        out.push_str("</div></nav>");
        out
    }

    fn breadcrumbs(&self, item: &Item) -> String {
        let krate = self.krate();
        let mut parts =
            vec![(krate.name.clone(), format!("{}{}/index.html", self.root, krate.name))];
        let mut path = format!("{}{}", self.root, krate.name);
        for module in &item.modules {
            path.push('/');
            path.push_str(module);
            parts.push((module.clone(), format!("{path}/index.html")));
        }
        if item.kind == Kind::Module && item.name.is_empty() {
            return String::new();
        }
        let mut out = String::from("<div class=\"breadcrumbs\">");
        for (at, (name, href)) in parts.iter().enumerate() {
            if at > 0 {
                out.push_str("::");
            }
            let _ = write!(out, "<a href=\"{}\">{}</a>", escape(href), escape(name));
        }
        out.push_str("</div>");
        out
    }

    fn notes(&self, notes: &Notes, in_list: bool) -> String {
        let mut out = String::new();
        if let Some(deprecated) = &notes.deprecated {
            if in_list {
                out.push_str("<span class=\"stab deprecated\" title=\"\">Deprecated</span>");
            } else {
                out.push_str("<div class=\"stab deprecated\"><span class=\"emoji\">👎</span><span>Deprecated");
                if let Some(since) = &deprecated.since {
                    let _ = write!(out, " since {}", escape(since));
                }
                if let Some(note) = &deprecated.note {
                    let doc = Doc { markdown: note.clone(), ..Doc::default() };
                    let _ = write!(out, ": {}", self.summary_html(&doc));
                }
                out.push_str("</span></div>");
            }
        }
        if let Some(cfg) = &notes.cfg {
            if in_list {
                let _ = write!(out, "<span class=\"stab portability\">{}</span>", short_cfg(cfg));
            } else {
                let _ = write!(
                    out,
                    "<div class=\"stab portability\">Available on <strong>{cfg}</strong> only.</div>"
                );
            }
        }
        if notes.is_unsafe && in_list {
            out.push_str("<sup title=\"unsafe function\">⚠</sup>");
        }
        out
    }

    fn item_page(&self, at: usize) -> String {
        let krate = self.krate();
        let item = &krate.items[at];
        let is_root = item.kind == Kind::Module && item.name.is_empty();
        let path = if is_root {
            krate.name.clone()
        } else {
            format!("{}::{}", krate.name, item.full_path())
        };
        let parent_path = std::iter::once(krate.name.as_str())
            .chain(item.modules.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("::");
        let title = if is_root {
            format!("{} - Rust", krate.name)
        } else {
            format!("{} in {} - Rust", item.name, parent_path)
        };
        let description = markdown::plain_summary(&item.docs.markdown, 160);
        let description = if description.is_empty() {
            format!("API documentation for the Rust `{}` {}", item.name, item.kind.short())
        } else {
            description
        };
        let sidebar_js = if is_root {
            None
        } else if item.kind == Kind::Module {
            Some("../sidebar-items.js")
        } else {
            Some("sidebar-items.js")
        };
        let mut out =
            self.head(&title, &description, &format!("blaze {}", item.kind.short()), sidebar_js);

        let location = if is_root {
            format!("Crate {}", escape(&krate.name))
        } else {
            format!("{} {}", item.kind.title(), escape(&item.name))
        };
        let sections = self.sidebar_sections(item);
        let module = if is_root {
            None
        } else {
            let (href, label) = if item.kind == Kind::Module {
                ("../index.html".to_owned(), parent_path.clone())
            } else {
                ("index.html".to_owned(), parent_path.clone())
            };
            let label = if item.modules.is_empty() {
                format!("In crate {}", escape(&label))
            } else {
                format!("In {}", escape(&label))
            };
            Some((href, label))
        };
        out.push_str(&self.sidebar(&location, &sections, module));
        out.push_str("<main><div class=\"width-limiter\">");
        out.push_str(&self.topbar());
        out.push_str("<section id=\"main-content\" class=\"content\">");

        // Heading.
        out.push_str("<div class=\"main-heading\">");
        out.push_str(&self.breadcrumbs(item));
        let heading_name = if is_root { krate.name.as_str() } else { item.name.as_str() };
        let _ = write!(
            out,
            "<h1>{} <span class=\"{}\">{}</span><button id=\"copy-path\" title=\"Copy item path to clipboard\" data-path=\"{}\">Copy item path</button></h1>",
            if is_root { "Crate" } else { item.kind.title() },
            item.kind.short(),
            escape(heading_name),
            escape(&path),
        );
        if let Some(src) = self.src_link(item.source) {
            let _ = write!(
                out,
                "<span class=\"sub-heading\"><a class=\"src\" href=\"{}\">Source</a></span>",
                escape(&src)
            );
        }
        out.push_str("</div>");

        if !item.decl.is_empty() {
            let _ = write!(
                out,
                "<pre class=\"rust item-decl\"><code>{}</code></pre>",
                self.code(&item.decl, None)
            );
        }
        out.push_str(&self.notes(&item.notes, false));
        if !item.docs.is_empty() {
            let _ = write!(
                out,
                "<details class=\"toggle top-doc\" open><summary class=\"hideme\"><span>Expand description</span></summary><div class=\"docblock\">{}</div></details>",
                self.docs_html(&item.docs, 1)
            );
        }
        match item.kind {
            Kind::Module => out.push_str(&self.module_body(item)),
            Kind::Struct | Kind::Union => {
                out.push_str(&self.fields_section(item));
                out.push_str(&self.impls_sections(item));
            }
            Kind::Enum => {
                out.push_str(&self.variants_section(item));
                out.push_str(&self.impls_sections(item));
            }
            Kind::Trait => out.push_str(&self.trait_body(item)),
            Kind::TypeAlias => {
                if let Some(aliased) = &item.aliased {
                    out.push_str(&self.section_header("aliased-type", "Aliased Type"));
                    let _ = write!(
                        out,
                        "<pre class=\"rust item-decl\"><code>{}</code></pre>",
                        self.code(aliased, None)
                    );
                    out.push_str(&self.fields_section(item));
                    out.push_str(&self.variants_section(item));
                }
            }
            _ => {}
        }
        out.push_str("</section><section id=\"search\" class=\"content hidden\"></section></div></main></body></html>");
        out
    }

    fn sidebar_sections(&self, item: &Item) -> Vec<(String, Vec<(String, String)>)> {
        let krate = self.krate();
        let mut out = vec![];
        let anchors = |members: &[Member]| -> Vec<(String, String)> {
            members
                .iter()
                .map(|m| (format!("#{}.{}", m.kind.short(), m.name), m.name.clone()))
                .collect()
        };
        match item.kind {
            Kind::Module => {
                let mut kinds = vec![];
                if !item.reexports.is_empty() {
                    kinds.push(("#reexports".to_owned(), "Re-exports".to_owned()));
                }
                for kind in Kind::MODULE_ORDER {
                    if item.children.iter().any(|&at| krate.items[at].kind == kind) {
                        kinds.push((format!("#{}", kind.section_id()), kind.plural().to_owned()));
                    }
                }
                let title = if item.name.is_empty() { "Crate Items" } else { "Module Items" };
                out.push((title.to_owned(), kinds));
            }
            Kind::Struct | Kind::Union => {
                out.push(("Fields".to_owned(), anchors(&item.fields)));
            }
            Kind::Enum => {
                out.push(("Variants".to_owned(), anchors(&item.variants)));
            }
            Kind::TypeAlias if item.aliased.is_some() => {
                out.push((
                    "Aliased Type".to_owned(),
                    vec![("#aliased-type".to_owned(), String::new())],
                ));
                out.push(("Fields".to_owned(), anchors(&item.fields)));
                out.push(("Variants".to_owned(), anchors(&item.variants)));
            }
            Kind::Trait => {
                let pick = |kind: Kind, provided: bool| -> Vec<(String, String)> {
                    item.trait_items
                        .iter()
                        .filter(|m| {
                            m.kind == kind && (kind != Kind::AssocType || m.provided == provided)
                        })
                        .map(|m| (format!("#{}.{}", m.kind.short(), m.name), m.name.clone()))
                        .collect()
                };
                out.push(("Required Associated Types".to_owned(), pick(Kind::AssocType, false)));
                out.push(("Provided Associated Types".to_owned(), pick(Kind::AssocType, true)));
                out.push(("Associated Constants".to_owned(), pick(Kind::AssocConst, false)));
                out.push(("Required Methods".to_owned(), pick(Kind::TyMethod, false)));
                out.push(("Provided Methods".to_owned(), pick(Kind::Method, true)));
                let foreign: Vec<(String, String)> = item
                    .impls
                    .foreign
                    .iter()
                    .map(|imp| {
                        let header = imp.header.plain();
                        let first = header.lines().next().unwrap_or_default();
                        let ty = first.rsplit_once(" for ").map_or(first, |(_, ty)| ty);
                        (format!("#{}", imp.anchor), ty.to_owned())
                    })
                    .collect();
                out.push(("Implementations on Foreign Types".to_owned(), foreign));
                let implementors: Vec<(String, String)> = if item.impls.implementors.is_empty()
                    && item.impls.blanket_implementors.is_empty()
                {
                    vec![]
                } else {
                    vec![("#implementors".to_owned(), "Implementors".to_owned())]
                };
                out.push(("Implementors".to_owned(), implementors));
            }
            _ => {}
        }
        let methods: Vec<(String, String)> = item
            .impls
            .inherent
            .iter()
            .flat_map(|imp| imp.items.iter())
            .filter(|m| m.kind == Kind::Method)
            .map(|m| (format!("#method.{}", m.name), m.name.clone()))
            .collect();
        out.push(("Methods".to_owned(), methods));
        for deref in &item.impls.deref {
            let title = format!("Methods from Deref<Target = {}>", deref.target.plain());
            let links = deref
                .members
                .iter()
                .map(|m| (format!("#method.{}", m.name), m.name.clone()))
                .collect();
            out.push((title, links));
        }
        let traits = |impls: &[ImplDoc]| -> Vec<(String, String)> {
            impls
                .iter()
                .map(|imp| {
                    let name = imp.trait_name.clone().unwrap_or_default();
                    let name = if imp.negative { format!("!{name}") } else { name };
                    (format!("#{}", imp.anchor), name)
                })
                .collect()
        };
        out.push(("Trait Implementations".to_owned(), traits(&item.impls.traits)));
        out.push(("Auto Trait Implementations".to_owned(), traits(&item.impls.auto)));
        out.push(("Blanket Implementations".to_owned(), traits(&item.impls.blanket)));
        out
    }

    fn section_header(&self, id: &str, title: &str) -> String {
        format!(
            "<h2 id=\"{id}\" class=\"section-header\">{title}<a href=\"#{id}\" class=\"anchor\">§</a></h2>"
        )
    }

    fn module_body(&self, module: &Item) -> String {
        let krate = self.krate();
        let mut out = String::new();
        if !module.reexports.is_empty() {
            out.push_str(&self.section_header("reexports", "Re-exports"));
            out.push_str("<dl class=\"item-table reexports\">");
            for reexport in &module.reexports {
                let _ = write!(
                    out,
                    "<dt id=\"reexport.{name}\"><code>{decl}</code></dt><dd>{summary}</dd>",
                    name = escape(&reexport.name),
                    decl = self.code(&reexport.decl, None),
                    summary = self.summary_html(&reexport.docs),
                );
            }
            out.push_str("</dl>");
        }
        for kind in Kind::MODULE_ORDER {
            let mut children: Vec<&Item> = module
                .children
                .iter()
                .map(|&at| &krate.items[at])
                .filter(|it| it.kind == kind)
                .collect();
            if children.is_empty() {
                continue;
            }
            children.sort_by(|a, b| a.name.cmp(&b.name));
            children.dedup_by(|a, b| a.name == b.name);
            out.push_str(&self.section_header(kind.section_id(), kind.plural()));
            out.push_str("<dl class=\"item-table\">");
            for child in children {
                let href = if kind == Kind::Module {
                    format!("{}/index.html", child.name)
                } else {
                    format!("{}.{}.html", kind.short(), child.name)
                };
                let full = format!("{}::{}", krate.name, child.full_path());
                let deprecated = if child.notes.deprecated.is_some() { " deprecated" } else { "" };
                let _ = write!(
                    out,
                    "<dt><a class=\"{kind}{deprecated}\" href=\"{href}\" title=\"{kind} {full}\">{name}</a>{notes}</dt><dd>{summary}</dd>",
                    kind = kind.short(),
                    href = escape(&href),
                    full = escape(&full),
                    name = escape(&child.name),
                    notes = self.notes(&child.notes, true),
                    summary = self.summary_html(&child.docs),
                );
            }
            out.push_str("</dl>");
        }
        out
    }

    fn fields_section(&self, item: &Item) -> String {
        let mut out = String::new();
        if item.fields.is_empty() {
            return out;
        }
        out.push_str(&self.section_header("fields", "Fields"));
        for field in &item.fields {
            let id = format!("structfield.{}", field.name);
            let _ = write!(
                out,
                "<span id=\"{id}\" class=\"structfield section-header\"><a href=\"#{id}\" class=\"anchor field\">§</a><code>{}</code></span>{}",
                self.code(&field.decl, None),
                self.notes(&field.notes, false),
            );
            if !field.docs.is_empty() {
                let _ =
                    write!(out, "<div class=\"docblock\">{}</div>", self.docs_html(&field.docs, 3));
            }
        }
        out
    }

    fn variants_section(&self, item: &Item) -> String {
        let mut out = String::new();
        if item.variants.is_empty() {
            return out;
        }
        out.push_str(&self.section_header("variants", "Variants"));
        out.push_str("<div class=\"variants\">");
        for variant in &item.variants {
            let id = format!("variant.{}", variant.name);
            let src = self
                .src_link(variant.source)
                .map(|it| format!("<a class=\"src rightside\" href=\"{}\">Source</a>", escape(&it)))
                .unwrap_or_default();
            let _ = write!(
                out,
                "<section id=\"{id}\" class=\"variant\">{src}<a href=\"#{id}\" class=\"anchor\">§</a><h3 class=\"code-header\">{}</h3></section>{}",
                self.code(&variant.decl, None),
                self.notes(&variant.notes, false),
            );
            if !variant.docs.is_empty() {
                let _ = write!(
                    out,
                    "<div class=\"docblock\">{}</div>",
                    self.docs_html(&variant.docs, 3)
                );
            }
            if !variant.fields.is_empty() {
                let _ =
                    write!(out, "<div class=\"sub-variant\" id=\"{id}.fields\"><h4>Fields</h4>");
                for field in &variant.fields {
                    let fid = format!("{id}.field.{}", field.name);
                    let _ = write!(
                        out,
                        "<div class=\"sub-variant-field\"><span id=\"{fid}\" class=\"section-header\"><a href=\"#{fid}\" class=\"anchor field\">§</a><code>{}</code></span>",
                        self.code(&field.decl, None)
                    );
                    if !field.docs.is_empty() {
                        let _ = write!(
                            out,
                            "<div class=\"docblock\">{}</div>",
                            self.docs_html(&field.docs, 4)
                        );
                    }
                    out.push_str("</div>");
                }
                out.push_str("</div>");
            }
        }
        out.push_str("</div>");
        out
    }

    fn impls_sections(&self, item: &Item) -> String {
        let mut out = String::new();
        if !item.impls.inherent.is_empty() {
            out.push_str(&self.section_header("implementations", "Implementations"));
            out.push_str("<div id=\"implementations-list\">");
            for imp in &item.impls.inherent {
                out.push_str(&self.impl_block(imp, true, true));
            }
            out.push_str("</div>");
        }
        for deref in &item.impls.deref {
            let anchor = self.ids.borrow_mut().unique(&deref.anchor);
            let list = self.ids.borrow_mut().unique(&deref.anchor);
            let _ = write!(
                out,
                "<details class=\"toggle big-toggle\" open><summary><h2 id=\"{anchor}\" class=\"section-header\"><span>Methods from {}&lt;Target = {}&gt;</span><a href=\"#{anchor}\" class=\"anchor\">§</a></h2></summary><div id=\"{list}\" class=\"impl-items\">",
                self.code(&deref.trait_, None),
                self.code(&deref.target, None),
            );
            for member in &deref.members {
                out.push_str(&self.member(member, true));
            }
            out.push_str("</div></details>");
        }
        let lists: [(&str, &str, &[ImplDoc], bool); 3] = [
            ("trait-implementations", "Trait Implementations", &item.impls.traits, true),
            ("synthetic-implementations", "Auto Trait Implementations", &item.impls.auto, false),
            ("blanket-implementations", "Blanket Implementations", &item.impls.blanket, false),
        ];
        for (id, title, impls, open) in lists {
            if impls.is_empty() {
                continue;
            }
            out.push_str(&self.section_header(id, title));
            let _ = write!(out, "<div id=\"{id}-list\">");
            for imp in impls {
                out.push_str(&self.impl_block(imp, open, false));
            }
            out.push_str("</div>");
        }
        out
    }

    fn impl_block(&self, imp: &ImplDoc, open: bool, inherent: bool) -> String {
        let mut out = String::new();
        let anchor = self.ids.borrow_mut().unique(&imp.anchor);
        let src = self
            .src_link(imp.source)
            .map(|it| format!("<a class=\"src rightside\" href=\"{}\">Source</a>", escape(&it)))
            .unwrap_or_default();
        let header = format!(
            "<section id=\"{anchor}\" class=\"impl\">{src}<a href=\"#{anchor}\" class=\"anchor\">§</a><h3 class=\"code-header\">{}</h3></section>",
            self.code(&imp.header, None)
        );
        if imp.items.is_empty() && imp.docs.is_empty() {
            out.push_str(&header);
            return out;
        }
        let _ = write!(
            out,
            "<details class=\"toggle implementors-toggle\"{}><summary>{header}</summary>",
            if open { " open" } else { "" }
        );
        if !imp.docs.is_empty() {
            let _ = write!(out, "<div class=\"docblock\">{}</div>", self.docs_html(&imp.docs, 3));
        }
        out.push_str("<div class=\"impl-items\">");
        for member in &imp.items {
            out.push_str(&self.member(member, inherent));
        }
        out.push_str("</div></details>");
        out
    }

    /// An associated item in an impl or trait: its header, linked to its own anchor, and docs.
    fn member(&self, member: &Member, docs_open: bool) -> String {
        let mut out = String::new();
        let id = self.ids.borrow_mut().unique(&format!("{}.{}", member.kind.short(), member.name));
        let src = self
            .src_link(member.source)
            .map(|it| format!("<a class=\"src rightside\" href=\"{}\">Source</a>", escape(&it)))
            .unwrap_or_default();
        let class = match member.kind {
            Kind::AssocType => "associatedtype",
            Kind::AssocConst => "associatedconstant",
            Kind::TyMethod => "tymethod",
            _ => "method",
        };
        let header = format!(
            "<section id=\"{id}\" class=\"{class}\">{src}<a href=\"#{id}\" class=\"anchor\">§</a><h4 class=\"code-header\">{}</h4></section>",
            self.code(&member.decl, Some(&id))
        );
        let notes = self.notes(&member.notes, false);
        if member.docs.is_empty() && notes.is_empty() {
            out.push_str(&header);
            return out;
        }
        let docs = if member.docs_from_trait {
            let summary = self.summary_html(&member.docs);
            let more = crate::model::first_paragraph(&member.docs.markdown).len()
                < member.docs.markdown.trim().len();
            if more {
                format!("<p>{summary} <a class=\"read-more\" href=\"#{id}\">Read more</a></p>")
            } else {
                format!("<p>{summary}</p>")
            }
        } else {
            self.docs_html(&member.docs, 4)
        };
        let _ = write!(
            out,
            "<details class=\"toggle method-toggle\"{}><summary>{header}</summary>{notes}<div class=\"docblock\">{docs}</div></details>",
            if docs_open { " open" } else { "" }
        );
        out
    }

    fn trait_body(&self, item: &Item) -> String {
        let mut out = String::new();
        let groups: [(&str, &str, fn(&Member) -> bool); 5] = [
            (
                "required-associated-types",
                "Required Associated Types",
                |m: &Member| m.kind == Kind::AssocType && !m.provided,
            ),
            (
                "provided-associated-types",
                "Provided Associated Types",
                |m: &Member| m.kind == Kind::AssocType && m.provided,
            ),
            ("associated-constants", "Associated Constants", |m: &Member| m.kind == Kind::AssocConst),
            ("required-methods", "Required Methods", |m: &Member| m.kind == Kind::TyMethod),
            ("provided-methods", "Provided Methods", |m: &Member| m.kind == Kind::Method),
        ];
        for (id, title, wanted) in groups {
            let members: Vec<&Member> = item.trait_items.iter().filter(|m| wanted(m)).collect();
            if members.is_empty() {
                continue;
            }
            out.push_str(&self.section_header(id, title));
            let _ = write!(out, "<div class=\"methods\">");
            for member in members {
                out.push_str(&self.member(member, true));
            }
            out.push_str("</div>");
        }
        if let Some(compatible) = item.dyn_compatible {
            out.push_str(&self.section_header("dyn-compatibility", "Dyn Compatibility"));
            let _ = write!(
                out,
                "<div class=\"dyn-compatibility-info\"><p>This trait is <b>{}</b> <a href=\"https://doc.rust-lang.org/nightly/reference/items/traits.html#dyn-compatibility\">dyn compatible</a>.</p></div>",
                if compatible { "" } else { "not" }
            );
        }
        if !item.impls.foreign.is_empty() {
            out.push_str(&self.section_header("foreign-impls", "Implementations on Foreign Types"));
            for imp in &item.impls.foreign {
                out.push_str(&self.impl_block(imp, false, false));
            }
        }
        if !item.impls.implementors.is_empty() || !item.impls.blanket_implementors.is_empty() {
            out.push_str(&self.section_header("implementors", "Implementors"));
            out.push_str("<div id=\"implementors-list\">");
            for imp in item.impls.implementors.iter().chain(&item.impls.blanket_implementors) {
                out.push_str(&self.impl_block(imp, false, false));
            }
            out.push_str("</div>");
        }
        out
    }

    fn all_items(&self) -> String {
        let krate = self.krate();
        let mut out = self.head("List of all items in this crate", "", "blaze all", None);
        out.push_str(&self.sidebar("All items", &[], None));
        out.push_str("<main><div class=\"width-limiter\">");
        out.push_str(&self.topbar());
        out.push_str("<section id=\"main-content\" class=\"content\"><div class=\"main-heading\"><h1>List of all items</h1></div>");
        let mut by_kind: HashMap<Kind, Vec<(String, String)>> = HashMap::default();
        for item in &krate.items {
            if item.kind == Kind::Module && item.name.is_empty() {
                continue;
            }
            let href = page_path(krate, item);
            let href = href.strip_prefix(&format!("{}/", krate.name)).unwrap_or(&href).to_owned();
            by_kind.entry(item.kind).or_default().push((item.full_path(), href));
        }
        for kind in Kind::MODULE_ORDER {
            let Some(mut items) = by_kind.remove(&kind) else { continue };
            items.sort();
            items.dedup();
            let _ = write!(
                out,
                "<h3 id=\"{}\">{}</h3><ul class=\"all-items\">",
                kind.section_id(),
                kind.plural()
            );
            for (path, href) in items {
                let _ = write!(out, "<li><a href=\"{}\">{}</a></li>", escape(&href), escape(&path));
            }
            out.push_str("</ul>");
        }
        out.push_str("</section><section id=\"search\" class=\"content hidden\"></section></div></main></body></html>");
        out
    }

    fn source_page(&self, file_at: usize) -> String {
        let krate = self.krate();
        let file = &krate.sources[file_at];
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let mut out = self.head(
            &format!("{name} - source"),
            &format!("Source of the Rust file `{}`.", file.path),
            "blaze src",
            None,
        );
        out.push_str(&self.sidebar(&format!("Source of {}", escape(&krate.name)), &[], None));
        out.push_str("<main><div class=\"width-limiter\">");
        out.push_str(&self.topbar());
        let _ = write!(
            out,
            "<section id=\"main-content\" class=\"content src-page\"><div class=\"main-heading\"><h1><a href=\"{root}{krate}/index.html\">{krate}</a>/{path}</h1></div>",
            root = self.root,
            krate = escape(&krate.name),
            path = escape(&file.path),
        );
        let highlighted = crate::highlight::rust(&file.text);
        let lines = split_lines(&highlighted);
        let digits = lines.len().to_string().len().max(2);
        let _ = write!(
            out,
            "<div class=\"example-wrap source digits-{digits}\"><pre class=\"rust\"><code>"
        );
        for (at, line) in lines.iter().enumerate() {
            let n = at + 1;
            let _ = write!(
                out,
                "<span class=\"src-line\" data-line=\"{n}\"><a class=\"line-number\" id=\"{n}\" href=\"#{n}\">{n}</a>{line}</span>"
            );
        }
        out.push_str("</code></pre></div></section><section id=\"search\" class=\"content hidden\"></section></div></main></body></html>");
        out
    }
}

fn name_class(kind: Kind) -> &'static str {
    match kind {
        Kind::Method | Kind::TyMethod | Kind::Function => "fn",
        Kind::AssocType => "associatedtype",
        Kind::AssocConst => "constant",
        other => other.short(),
    }
}

/// A `cfg` note shortened for a list: the words without the HTML.
fn short_cfg(cfg: &str) -> String {
    cfg.to_owned()
}

/// Highlighted HTML split into lines, a `<span>` open across a line break closed and opened
/// again so each line stands alone.
fn split_lines(html: &str) -> Vec<String> {
    let mut lines = vec![];
    let mut current = String::new();
    let mut open: Option<String> = None;
    let mut rest = html;
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix("<span") {
            let end = stripped.find('>').map_or(rest.len(), |it| it + 5 + 1);
            let tag = &rest[..end];
            current.push_str(tag);
            open = Some(tag.to_owned());
            rest = &rest[end..];
        } else if let Some(stripped) = rest.strip_prefix("</span>") {
            current.push_str("</span>");
            open = None;
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix('\n') {
            if open.is_some() {
                current.push_str("</span>");
            }
            lines.push(std::mem::take(&mut current));
            if let Some(tag) = &open {
                current.push_str(tag);
            }
            rest = stripped;
        } else {
            let next = rest.find(['<', '\n']).unwrap_or(rest.len()).max(1);
            current.push_str(&rest[..next]);
            rest = &rest[next..];
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::split_lines;

    #[test]
    fn spans_across_lines_are_closed_and_reopened() {
        let lines = split_lines("a<span class=\"comment\">/* x\ny */</span>b");
        assert_eq!(
            lines,
            vec![
                "a<span class=\"comment\">/* x</span>".to_owned(),
                "<span class=\"comment\">y */</span>b".to_owned()
            ]
        );
    }
}
