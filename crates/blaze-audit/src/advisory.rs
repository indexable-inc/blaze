//! RustSec advisories: the format of `github.com/rustsec/advisory-db`, read from a directory
//! a driver lists, never fetched here.
//!
//! An advisory is a markdown file `crates/<package>/<ID>.md` that opens with a TOML front
//! matter in a ```` ```toml ```` fence:
//!
//! ```text
//! ```toml
//! [advisory]
//! id = "RUSTSEC-2021-0001"        package = "foo"            date = "2021-01-01"
//! url, references, categories, keywords, aliases, related, cvss, license, source
//! informational = "unmaintained" | "unsound" | "notice"     withdrawn = "YYYY-MM-DD"
//! [affected]  functions = { "foo::bar" = ["< 1.2.3"] }   arch = [..]   os = [..]
//! [versions]  patched = [">= 1.2.3"]   unaffected = ["< 1.0.0"]
//! ```
//! # Title
//! Description.
//! ```
//!
//! The files are bytes blaze did not write: each is bounded before it is decoded, and so is
//! the database; a key this does not know is refused by name, as is an id or a path that does
//! not say what the file is, so an advisory is never half read.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

use semver::VersionReq;

use crate::date::Date;

/// The largest advisory file read: 256 KiB. Real ones are a few KiB; the largest some tens.
pub const MOST_ADVISORY_BYTES: usize = 256 << 10;

/// The most advisory files a database may hold: the RustSec database has some thousands;
/// 2^16 is an order past it, refused before any file is decoded.
pub const MOST_ADVISORIES: usize = 1 << 16;

/// The most bytes a database's advisories may hold together: 256 MiB.
pub const MOST_DATABASE_BYTES: usize = 256 << 20;

/// What an informational advisory says.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Informational {
    Unmaintained,
    Unsound,
    Notice,
}

impl Informational {
    pub fn name(self) -> &'static str {
        match self {
            Informational::Unmaintained => "unmaintained",
            Informational::Unsound => "unsound",
            Informational::Notice => "notice",
        }
    }
}

/// One advisory.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Advisory {
    pub id: String,
    pub package: String,
    pub date: Date,
    pub title: String,
    pub description: String,
    pub url: Option<String>,
    pub references: Vec<String, Global>,
    pub categories: Vec<String, Global>,
    pub keywords: Vec<String, Global>,
    pub aliases: Vec<String, Global>,
    pub related: Vec<String, Global>,
    pub cvss: Option<String>,
    pub license: Option<String>,
    pub source: Option<String>,
    pub informational: Option<Informational>,
    pub withdrawn: Option<Date>,
    /// Each function path with the ranges it is affected in, sorted by path.
    pub functions: Vec<(String, Vec<String, Global>), Global>,
    pub arch: Vec<String, Global>,
    pub os: Vec<String, Global>,
    /// The ranges as written (rendered back as written), and parsed.
    pub patched: Vec<String, Global>,
    pub unaffected: Vec<String, Global>,
    pub patched_reqs: Vec<VersionReq, Global>,
    pub unaffected_reqs: Vec<VersionReq, Global>,
}

/// Why an advisory, or a database, was not read. `file` is the path within the database.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdvisoryRefused {
    TooBig { file: String, bytes: usize },
    TooMany { files: usize },
    DatabaseTooBig { bytes: usize },
    /// `file` breaks the format: `why` names the field or the part.
    Bad { file: String, why: String },
}

impl fmt::Display for AdvisoryRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdvisoryRefused::TooBig { file, bytes } => write!(
                f,
                "the advisory `{file}` is {bytes} bytes, more than the {MOST_ADVISORY_BYTES} an advisory may be"
            ),
            AdvisoryRefused::TooMany { files } => write!(
                f,
                "the advisory database has {files} advisories, more than the {MOST_ADVISORIES} it may"
            ),
            AdvisoryRefused::DatabaseTooBig { bytes } => write!(
                f,
                "the advisory database's advisories hold {bytes} bytes, more than the {MOST_DATABASE_BYTES} they may"
            ),
            AdvisoryRefused::Bad { file, why } => write!(f, "the advisory `{file}` {why}"),
        }
    }
}

const FENCE: &str = "```toml\n";
const FENCE_END: &str = "\n```\n";

/// Whether `id` is a RustSec id, `RUSTSEC-YYYY-NNNN`.
pub fn is_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    id.len() == 17
        && id.starts_with("RUSTSEC-")
        && bytes[12] == b'-'
        && bytes[8..12].iter().chain(&bytes[13..]).all(u8::is_ascii_digit)
}

/// Whether `name` could be a crate's name: what a directory of `crates/` must be.
fn is_package(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'-' || it == b'_')
}

struct Fields<'a> {
    file: &'a str,
    table: &'a toml::Table,
    part: &'a str,
}

impl<'a> Fields<'a> {
    fn bad(&self, why: String) -> AdvisoryRefused {
        AdvisoryRefused::Bad { file: self.file.to_owned(), why }
    }

    /// Every key of the table is one of `known`.
    fn only(&self, known: &[&str]) -> Result<(), AdvisoryRefused> {
        match self.table.keys().find(|key| !known.contains(&key.as_str())) {
            Some(key) => Err(self.bad(format!(
                "has `{}.{key}`, which is not a field of the format (`{}` are)",
                self.part,
                known.join("`, `")
            ))),
            None => Ok(()),
        }
    }

    fn text(&self, key: &str) -> Result<Option<String>, AdvisoryRefused> {
        match self.table.get(key) {
            None => Ok(None),
            Some(toml::Value::String(it)) => Ok(Some(it.clone())),
            Some(_) => Err(self.bad(format!("has `{}.{key}`, which is not a string", self.part))),
        }
    }

    fn need(&self, key: &str) -> Result<String, AdvisoryRefused> {
        self.text(key)?.ok_or_else(|| self.bad(format!("has no `{}.{key}`", self.part)))
    }

    fn date(&self, key: &str) -> Result<Option<Date>, AdvisoryRefused> {
        match self.table.get(key) {
            None => Ok(None),
            Some(toml::Value::String(it)) => {
                Date::parse(it).map(Some).map_err(|why| self.bad(format!("has `{}.{key}`: {why}", self.part)))
            }
            // TOML's own date, as `date = 2021-01-01` unquoted.
            Some(toml::Value::Datetime(it)) => {
                let text = it.to_string();
                Date::parse(&text).map(Some).map_err(|_| self.bad(format!("has `{}.{key}` = {text}, which is no day", self.part)))
            }
            Some(_) => Err(self.bad(format!("has `{}.{key}`, which is not a day", self.part))),
        }
    }

    fn list(&self, key: &str) -> Result<Vec<String, Global>, AdvisoryRefused> {
        let mut out = Vec::new_in(Global);
        match self.table.get(key) {
            None => {}
            Some(toml::Value::Array(items)) => {
                for item in items {
                    let text = item
                        .as_str()
                        .ok_or_else(|| self.bad(format!("has `{}.{key}` with an item that is not a string", self.part)))?;
                    out.push(text.to_owned());
                }
            }
            Some(_) => return Err(self.bad(format!("has `{}.{key}`, which is not a list of strings", self.part))),
        }
        Ok(out)
    }
}

fn reqs(file: &str, part: &str, ranges: &[String]) -> Result<Vec<VersionReq, Global>, AdvisoryRefused> {
    let mut out = Vec::with_capacity_in(ranges.len(), Global);
    for range in ranges {
        let req = VersionReq::parse(range).map_err(|why| AdvisoryRefused::Bad {
            file: file.to_owned(),
            why: format!("has `{part}` range `{range}`, which is no semver requirement: {why}"),
        })?;
        out.push(req);
    }
    Ok(out)
}

/// A TOML list of `items`.
fn strings(items: &[String]) -> toml::Value {
    let mut out = Vec::with_capacity_in(items.len(), Global);
    out.extend(items.iter().cloned().map(toml::Value::String));
    toml::Value::Array(out)
}

fn table_of<'a>(doc: &'a toml::Table, key: &str, file: &str) -> Result<Option<&'a toml::Table>, AdvisoryRefused> {
    match doc.get(key) {
        None => Ok(None),
        Some(toml::Value::Table(it)) => Ok(Some(it)),
        Some(_) => Err(AdvisoryRefused::Bad { file: file.to_owned(), why: format!("has `{key}`, which is not a table") }),
    }
}

impl Advisory {
    /// The advisory at `file` (its path within the database, `crates/<package>/<id>.md`).
    pub fn parse(file: &str, bytes: &[u8]) -> Result<Advisory, AdvisoryRefused> {
        let bad = |why: String| AdvisoryRefused::Bad { file: file.to_owned(), why };
        if bytes.len() > MOST_ADVISORY_BYTES {
            return Err(AdvisoryRefused::TooBig { file: file.to_owned(), bytes: bytes.len() });
        }
        let text = std::str::from_utf8(bytes).map_err(|_| bad("is not UTF-8".to_owned()))?;
        let text = text.replace("\r\n", "\n");
        let rest = text.strip_prefix(FENCE).ok_or_else(|| bad("does not open with a ```toml fence".to_owned()))?;
        let end = rest.find(FENCE_END).ok_or_else(|| bad("has no end to its ```toml fence".to_owned()))?;
        let (front, body) = (&rest[..end + 1], &rest[end + FENCE_END.len()..]);
        let doc: toml::Table = toml::from_str(front).map_err(|why| bad(format!("has front matter that is not TOML: {why}")))?;
        if let Some(key) = doc.keys().find(|key| !["advisory", "affected", "versions"].contains(&key.as_str())) {
            return Err(bad(format!("has `[{key}]`, which is not a table of the format (`advisory`, `affected`, `versions` are)")));
        }
        let head = table_of(&doc, "advisory", file)?.ok_or_else(|| bad("has no `[advisory]`".to_owned()))?;
        let head = Fields { file, table: head, part: "advisory" };
        head.only(&[
            "id", "package", "date", "url", "references", "categories", "keywords", "aliases", "related", "cvss",
            "license", "source", "informational", "withdrawn",
        ])?;
        let id = head.need("id")?;
        let package = head.need("package")?;
        if !is_id(&id) {
            return Err(bad(format!("has the id `{id}`, which is no RUSTSEC-YYYY-NNNN")));
        }
        if !is_package(&package) {
            return Err(bad(format!("names the package `{package}`, which is no crate name")));
        }
        let want = format!("crates/{package}/{id}.md");
        if file != want {
            return Err(bad(format!("is advisory `{id}` of `{package}`, which belongs at `{want}`")));
        }
        let date = head.date("date")?.ok_or_else(|| bad("has no `advisory.date`".to_owned()))?;
        let informational = match head.text("informational")?.as_deref() {
            None => None,
            Some("unmaintained") => Some(Informational::Unmaintained),
            Some("unsound") => Some(Informational::Unsound),
            Some("notice") => Some(Informational::Notice),
            Some(other) => {
                return Err(bad(format!(
                    "has `advisory.informational` = `{other}`, which is not known (`unmaintained`, `unsound` and `notice` are)"
                )));
            }
        };
        let mut functions = Vec::new_in(Global);
        let (mut arch, mut os) = (Vec::new_in(Global), Vec::new_in(Global));
        if let Some(affected) = table_of(&doc, "affected", file)? {
            let affected = Fields { file, table: affected, part: "affected" };
            affected.only(&["functions", "arch", "os"])?;
            arch = affected.list("arch")?;
            os = affected.list("os")?;
            if let Some(found) = table_of(affected.table, "functions", file)? {
                let found = Fields { file, table: found, part: "affected.functions" };
                for path in found.table.keys() {
                    let ranges = found.list(path)?;
                    reqs(file, &format!("affected.functions.{path}"), &ranges)?;
                    functions.push((path.clone(), ranges));
                }
            }
        }
        let (mut patched, mut unaffected) = (Vec::new_in(Global), Vec::new_in(Global));
        if let Some(versions) = table_of(&doc, "versions", file)? {
            let versions = Fields { file, table: versions, part: "versions" };
            versions.only(&["patched", "unaffected"])?;
            patched = versions.list("patched")?;
            unaffected = versions.list("unaffected")?;
        } else if informational.is_none() {
            return Err(bad("has no `[versions]`, which an advisory that is not informational needs".to_owned()));
        }
        let patched_reqs = reqs(file, "versions.patched", &patched)?;
        let unaffected_reqs = reqs(file, "versions.unaffected", &unaffected)?;
        let body = body.trim_start_matches('\n');
        let (title, description) = match body.strip_prefix("# ") {
            Some(rest) => {
                let (title, description) = rest.split_once('\n').unwrap_or((rest, ""));
                (title.trim().to_owned(), description.trim().to_owned())
            }
            None => return Err(bad("has no `# Title` after its front matter".to_owned())),
        };
        Ok(Advisory {
            id,
            package,
            date,
            title,
            description,
            url: head.text("url")?,
            references: head.list("references")?,
            categories: head.list("categories")?,
            keywords: head.list("keywords")?,
            aliases: head.list("aliases")?,
            related: head.list("related")?,
            cvss: head.text("cvss")?,
            license: head.text("license")?,
            source: head.text("source")?,
            informational,
            withdrawn: head.date("withdrawn")?,
            functions,
            arch,
            os,
            patched,
            unaffected,
            patched_reqs,
            unaffected_reqs,
        })
    }

    /// The advisory as a file of the format, with sorted keys: [`Advisory::parse`] of it is
    /// this advisory.
    pub fn render(&self) -> String {
        let mut head = toml::Table::new();
        let put = |table: &mut toml::Table, key: &str, value: Option<toml::Value>| {
            if let Some(value) = value {
                table.insert(key.to_owned(), value);
            }
        };
        let text = |it: &Option<String>| it.clone().map(toml::Value::String);
        let list = |it: &[String]| (!it.is_empty()).then(|| strings(it));
        put(&mut head, "id", Some(toml::Value::String(self.id.clone())));
        put(&mut head, "package", Some(toml::Value::String(self.package.clone())));
        put(&mut head, "date", Some(toml::Value::String(self.date.to_string())));
        put(&mut head, "url", text(&self.url));
        put(&mut head, "references", list(&self.references));
        put(&mut head, "categories", list(&self.categories));
        put(&mut head, "keywords", list(&self.keywords));
        put(&mut head, "aliases", list(&self.aliases));
        put(&mut head, "related", list(&self.related));
        put(&mut head, "cvss", text(&self.cvss));
        put(&mut head, "license", text(&self.license));
        put(&mut head, "source", text(&self.source));
        put(&mut head, "informational", self.informational.map(|it| toml::Value::String(it.name().to_owned())));
        put(&mut head, "withdrawn", self.withdrawn.map(|it| toml::Value::String(it.to_string())));
        let mut doc = toml::Table::new();
        doc.insert("advisory".to_owned(), toml::Value::Table(head));
        let mut affected = toml::Table::new();
        put(&mut affected, "arch", list(&self.arch));
        put(&mut affected, "os", list(&self.os));
        if !self.functions.is_empty() {
            let mut functions = toml::Table::new();
            for (path, ranges) in &self.functions {
                functions.insert(path.clone(), strings(ranges));
            }
            affected.insert("functions".to_owned(), toml::Value::Table(functions));
        }
        if !affected.is_empty() {
            doc.insert("affected".to_owned(), toml::Value::Table(affected));
        }
        if self.informational.is_none() || !self.patched.is_empty() || !self.unaffected.is_empty() {
            let mut versions = toml::Table::new();
            versions.insert("patched".to_owned(), strings(&self.patched));
            put(&mut versions, "unaffected", list(&self.unaffected));
            doc.insert("versions".to_owned(), toml::Value::Table(versions));
        }
        let front = toml::to_string(&doc).expect("a table of strings and lists renders");
        let mut out = format!("{FENCE}{front}");
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n# ");
        out.push_str(&self.title);
        out.push('\n');
        if !self.description.is_empty() {
            out.push('\n');
            out.push_str(&self.description);
            out.push('\n');
        }
        out
    }
}

/// A database's advisories, read: by package, then id; withdrawn ones kept apart.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Database {
    pub advisories: Vec<Advisory, Global>,
    /// Ids of the advisories withdrawn, which match nothing.
    pub withdrawn: Vec<String, Global>,
    /// The newest advisory's date: the database is at least as old as today less this.
    pub newest: Option<Date>,
}

impl Database {
    /// The advisories of `files` (each a path within the database and its bytes). Files not
    /// under `crates/` (the `rust/` advisories of the toolchain, the README, the repository's
    /// own files) are not crates' advisories and are passed over; every `.md` under `crates/`
    /// must be one.
    pub fn parse(files: &[(&str, &[u8])]) -> Result<Database, AdvisoryRefused> {
        let mut ours: Vec<&(&str, &[u8]), Global> = Vec::new_in(Global);
        ours.extend(files.iter().filter(|(path, _)| path.starts_with("crates/") && path.ends_with(".md")));
        if ours.len() > MOST_ADVISORIES {
            return Err(AdvisoryRefused::TooMany { files: ours.len() });
        }
        let bytes: usize = ours.iter().map(|(_, it)| it.len()).sum();
        if bytes > MOST_DATABASE_BYTES {
            return Err(AdvisoryRefused::DatabaseTooBig { bytes });
        }
        let mut advisories = Vec::with_capacity_in(ours.len(), Global);
        let mut withdrawn = Vec::new_in(Global);
        let mut newest = None;
        for (path, bytes) in ours {
            let advisory = Advisory::parse(path, bytes)?;
            newest = newest.max(Some(advisory.date));
            if advisory.withdrawn.is_some() {
                withdrawn.push(advisory.id);
            } else {
                advisories.push(advisory);
            }
        }
        advisories.sort_by(|a, b| (&a.package, &a.id).cmp(&(&b.package, &b.id)));
        let mut ids: Vec<(&str, &str), Global> = Vec::with_capacity_in(advisories.len(), Global);
        ids.extend(advisories.iter().map(|it| (it.id.as_str(), it.package.as_str())));
        ids.sort();
        if let Some(pair) = ids.windows(2).find(|it| it[0].0 == it[1].0) {
            return Err(AdvisoryRefused::Bad {
                file: format!("crates/{}/{}.md", pair[1].1, pair[1].0),
                why: format!("has the id `{}`, which the advisory of `{}` has too", pair[1].0, pair[0].1),
            });
        }
        withdrawn.sort();
        Ok(Database { advisories, withdrawn, newest })
    }

    /// The advisories of `package`.
    pub fn of(&self, package: &str) -> &[Advisory] {
        let start = self.advisories.partition_point(|it| it.package.as_str() < package);
        let end = self.advisories.partition_point(|it| it.package.as_str() <= package);
        &self.advisories[start..end]
    }
}
