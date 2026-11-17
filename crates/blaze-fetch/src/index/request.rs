//! The pure rules of the sparse protocol (cargo 1.99's `http_remote.rs` and cargo-util's
//! `make_dep_path` and `crate_url`; the parked blaze-registry's `sparse.rs` was the reference,
//! rewritten on this crate's rules): where a crate's file is, how it is asked for again, what an
//! answer's status means, the registry's `config.json`, and where the cache record of a file
//! lives under the directory grant.

#![blaze::sans_io]

use blaze_json::{Event, Reader, What};
use blaze_own::{Alloc, String};
use blaze_text::{Put, PutDisplay as _, Show, Writes as _};

use super::record::Validator;

/// crates.io's index, as a source names it (cargo's `CRATES_IO_HTTP_INDEX`).
pub const CRATES_IO_INDEX: &str = "sparse+https://index.crates.io/";

/// The headers of every index request (cargo's): the protocol's version, and plain text.
pub const INDEX_HEADERS: [(&str, &str); 2] = [("cargo-protocol", "version=1"), ("accept", "text/plain")];

/// The longest crate name asked for: crates.io's limit is 64; another registry's names are held
/// to this.
pub const MOST_NAME: usize = 256;

/// The folder, under blaze's home (`<home>/.blaze/v2/`), the index records are kept in:
/// beside the sources store (`sources`), one folder a registry.
pub const INDEX_FOLDER: &str = "index";

/// Why a name has no index file: no file of it could be named without leaving its folder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameRefused {
    Empty,
    TooLong { len: usize },
    /// A character no crate name has (only ASCII letters, digits, `-` and `_`).
    Char { at: usize, ch: char },
}

impl Show for NameRefused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match *self {
            NameRefused::Empty => out.str("an empty crate name"),
            NameRefused::TooLong { len } => out.str("a crate name of ").int(len).str(" bytes, past ").int(MOST_NAME),
            NameRefused::Char { at, ch } => out.str("a crate name with ").put_display(&ch.escape_debug()).str(" at byte ").int(at),
        };
    }
}

/// `name` checked to be a crate name: what keeps every path made of it inside its folder.
pub fn check_name(name: &str) -> Result<(), NameRefused> {
    if name.is_empty() {
        return Err(NameRefused::Empty);
    }
    if name.len() > MOST_NAME {
        return Err(NameRefused::TooLong { len: name.len() });
    }
    match name.char_indices().find(|(_, ch)| !(ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')) {
        Some((at, ch)) => Err(NameRefused::Char { at, ch }),
        None => Ok(()),
    }
}

/// The path of `name`'s index file, appended to `out`: its name in lower case under `1/`, `2/`,
/// `3/<first>/` or `<first two>/<next two>/` (cargo's `RegistryIndex::load_summaries` over
/// `make_dep_path`), so names that differ only in case are one file.
pub fn index_path_in(name: &str, out: &mut String<Alloc<'_>>) -> Result<(), NameRefused> {
    check_name(name)?;
    let start = out.len();
    dep_path_in(name, false, out);
    out.as_mut_str()[start..].make_ascii_lowercase();
    Ok(())
}

/// cargo-util's `make_dep_path` of a checked name, as given: with `prefix_only`, the folder
/// alone (a `dl` template's `{prefix}`).
fn dep_path_in(name: &str, prefix_only: bool, out: &mut String<Alloc<'_>>) {
    match name.len() {
        1 => out.str("1"),
        2 => out.str("2"),
        3 => out.str("3/").str(&name[..1]),
        _ => out.str(&name[..2]).str("/").str(&name[2..4]),
    };
    if !prefix_only {
        out.str("/").str(name);
    }
}

/// The request header that asks only for a change since `validator`: `If-None-Match` for an
/// `ETag`, `If-Modified-Since` for a `Last-Modified`; none for neither.
pub fn condition(validator: Validator<'_>) -> Option<(&'static str, &str)> {
    match validator {
        Validator::None => None,
        Validator::Etag(tag) => Some(("if-none-match", tag)),
        Validator::LastModified(time) => Some(("if-modified-since", time)),
    }
}

/// What to keep of a 200 answer to ask with next time: its `ETag`, else its `Last-Modified`
/// (cargo's order), each as sent less the blanks around it.
pub fn validator_of<'h>(etag: Option<&'h str>, last_modified: Option<&'h str>) -> Validator<'h> {
    match (etag.map(str::trim), last_modified.map(str::trim)) {
        (Some(tag), _) if !tag.is_empty() => Validator::Etag(tag),
        (_, Some(time)) if !time.is_empty() => Validator::LastModified(time),
        _ => Validator::None,
    }
}

/// An answer's header by its name, in any case (HTTP's names are caseless).
pub fn header<'h, S: AsRef<str>>(headers: &'h [(S, S)], name: &str) -> Option<&'h str> {
    headers.iter().find(|(it, _)| it.as_ref().eq_ignore_ascii_case(name)).map(|(_, value)| value.as_ref())
}

/// What an answer to an index request says, by its status (cargo's `http_remote.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnswerKind {
    /// 200: the file, and its validators.
    New,
    /// 304: the cached record stands.
    NotModified,
    /// 404, 410 or 451: the registry has no such crate. An answer, not an error.
    NoSuchCrate,
    /// 429 or 5xx: tried again by the retry policy.
    Transient,
    /// 401: the registry wants credentials, which this crate has none of.
    Unauthorized,
    /// Anything else: a typed failure naming the status.
    Other,
}

pub fn answer_kind(status: u16) -> AnswerKind {
    match status {
        200 => AnswerKind::New,
        304 => AnswerKind::NotModified,
        404 | 410 | 451 => AnswerKind::NoSuchCrate,
        401 => AnswerKind::Unauthorized,
        429 | 500..=599 => AnswerKind::Transient,
        _ => AnswerKind::Other,
    }
}

/// A registry, as its cache folder is named: the first 16 bytes of the BLAKE3 of its index URL
/// as a source names it (`sparse+https://index.crates.io/`), so two registries never share
/// records and no path or host of the machine is in the key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RegistryKey(pub [u8; 16]);

impl RegistryKey {
    pub fn of(index_url: &str) -> RegistryKey {
        let full = crate::Blake3::of(index_url.as_bytes()).0;
        let mut key = [0; 16];
        key.copy_from_slice(&full[..16]);
        RegistryKey(key)
    }
}

/// Where the record of an index file is kept, under blaze's home (`<home>/.blaze/v2/`):
/// `index/<registry, 32 hex>/<index path>`, appended to `out`. One record a (registry, file):
/// replaced whole (the directory grant's `publish`) each time the registry's file changes.
pub fn cache_path_in(registry: &RegistryKey, index_path: &str, out: &mut String<Alloc<'_>>) {
    out.str(INDEX_FOLDER).str("/").hex_bytes(&registry.0).str("/").str(index_path);
}

/// A registry's `config.json` (cargo's `RegistryConfig`): `dl`, `api` and `auth-required`;
/// other keys are the registry's and let be.
#[derive(Debug)]
pub struct RegistryConfig<'a> {
    /// Where a `.crate` is: a template of `{crate}`, `{version}`, `{prefix}`, `{lowerprefix}`
    /// and `{sha256-checksum}`, or, with none of them, a URL that `/{crate}/{version}/download`
    /// is added to ([`crate_url_in`]).
    pub dl: String<Alloc<'a>>,
    pub api: Option<String<Alloc<'a>>>,
    pub auth_required: bool,
}

impl RegistryConfig<'_> {
    /// Its name at the index's root.
    pub const NAME: &'static str = "config.json";
}

/// Why a `config.json` was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigRefused {
    Json { what: What, at: usize },
    NotAnObject,
    Missing { field: &'static str },
    WrongType { field: &'static str },
    Twice { field: &'static str },
}

impl Show for ConfigRefused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.str("the registry's config.json: ");
        match *self {
            ConfigRefused::Json { what, at } => out.str("not JSON at byte ").int(at).str(": ").put_display(&what),
            ConfigRefused::NotAnObject => out.str("not a JSON object"),
            ConfigRefused::Missing { field } => out.str("`").str(field).str("` is missing"),
            ConfigRefused::WrongType { field } => out.str("`").str(field).str("` is not of its type"),
            ConfigRefused::Twice { field } => out.str("`").str(field).str("` is given twice"),
        };
    }
}

/// `config.json` of 1 MiB at most.
const CONFIG_LIMITS: blaze_json::Limits = blaze_json::Limits {
    most_bytes: 1 << 20,
    most_depth: 32,
    most_number_len: 40,
    duplicates: blaze_json::Duplicates::Refuse,
};

/// `bytes`, a registry's `config.json`, read; its strings in `alloc`.
pub fn config_in<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<RegistryConfig<'a>, ConfigRefused> {
    let mut reader = Reader::new_in(bytes, CONFIG_LIMITS, alloc).map_err(config_json)?;
    if !matches!(next(&mut reader)?, Event::StartMap) {
        return Err(ConfigRefused::NotAnObject);
    }
    let (mut dl, mut api, mut auth_required) = (None, None, None);
    let mut key = String::new_in(alloc);
    loop {
        let Event::Key(name) = next(&mut reader)? else { break };
        key.clear();
        name.decode_into(&mut key);
        let value = next(&mut reader)?;
        match key.as_str() {
            "dl" => {
                let Event::Text(text) = value else { return Err(ConfigRefused::WrongType { field: "dl" }) };
                if dl.replace(text.to_string_in(alloc)).is_some() {
                    return Err(ConfigRefused::Twice { field: "dl" });
                }
            }
            "api" => {
                let found = match value {
                    Event::Text(text) => Some(text.to_string_in(alloc)),
                    Event::Null => None,
                    _ => return Err(ConfigRefused::WrongType { field: "api" }),
                };
                if api.replace(found).is_some() {
                    return Err(ConfigRefused::Twice { field: "api" });
                }
            }
            "auth-required" => {
                let Event::Bool(on) = value else { return Err(ConfigRefused::WrongType { field: "auth-required" }) };
                if auth_required.replace(on).is_some() {
                    return Err(ConfigRefused::Twice { field: "auth-required" });
                }
            }
            _ => {
                if matches!(value, Event::StartMap | Event::StartList) {
                    let base = reader.depth() - 1;
                    while reader.depth() > base {
                        next(&mut reader)?;
                    }
                }
            }
        }
    }
    let Some(dl) = dl else { return Err(ConfigRefused::Missing { field: "dl" }) };
    Ok(RegistryConfig { dl, api: api.flatten(), auth_required: auth_required.unwrap_or(false) })
}

fn config_json(error: blaze_json::Error) -> ConfigRefused {
    ConfigRefused::Json { what: error.what, at: error.at }
}

/// The next event of a `config.json`: inside its object there is always one.
fn next<'t>(reader: &mut Reader<'t, Alloc<'_>>) -> Result<Event<'t>, ConfigRefused> {
    match reader.next() {
        Ok(Some(item)) => Ok(item.event),
        Ok(None) => Err(ConfigRefused::NotAnObject),
        Err(error) => Err(config_json(error)),
    }
}

/// The URL of a `.crate` from a `dl` template (cargo-util's `crate_url`), appended to `out`:
/// each marker replaced, or, with none in it, `/{crate}/{version}/download` added. `name` is
/// checked first ([`check_name`]): its prefix is cut from it by bytes.
pub fn crate_url_in(
    template: &str,
    name: &str,
    version: &str,
    sha256: &[u8; 32],
    out: &mut String<Alloc<'_>>,
) -> Result<(), NameRefused> {
    const MARKERS: [&str; 5] = ["{crate}", "{version}", "{prefix}", "{lowerprefix}", "{sha256-checksum}"];
    check_name(name)?;
    if !MARKERS.iter().any(|marker| template.contains(marker)) {
        out.str(template).str("/").str(name).str("/").str(version).str("/download");
        return Ok(());
    }
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.str(&rest[..open]);
        rest = &rest[open..];
        let Some(marker) = MARKERS.iter().find(|marker| rest.starts_with(**marker)) else {
            out.str("{");
            rest = &rest[1..];
            continue;
        };
        match *marker {
            "{crate}" => {
                out.str(name);
            }
            "{version}" => {
                out.str(version);
            }
            "{prefix}" => dep_path_in(name, true, out),
            "{lowerprefix}" => {
                let start = out.len();
                dep_path_in(name, true, out);
                out.as_mut_str()[start..].make_ascii_lowercase();
            }
            _ => {
                out.hex_bytes(sha256);
            }
        }
        rest = &rest[marker.len()..];
    }
    out.str(rest);
    Ok(())
}
