//! The request certificate: the one record that says a request of a checkout is answered
//! already, and by what (docs/v2/plan/design/certificate.md).
//!
//! A certificate is written once a build answered ([`Certificate`]) and holds everything that
//! answer is a function of, each by content: the request (its shape, its config less the
//! environment, what it says past those, how its client is drawn), the blaze that served it,
//! the checkout's files as groups of the view's tree ([`Group`]: a directory's Merkle id or a
//! file's id, with the units that read it), the variables its steps read with the hash of
//! each value, the files outside every view its steps read with the hash of each (none: it
//! was not there, a negative probe), the outputs with the stamp each was placed under, the
//! units' record keys, and what the build said. It is a store record (`Kind::Query`), keyed
//! by content alone ([`key`]), so a restarted daemon and a second checkout of the same
//! content find it; the checkout's slot ([`slot`]) names the certificate its last build left,
//! so a request whose content has no certificate still learns which input moved.
//!
//! The check ([`Certificate::holds`]) is one record read, the id compares and the looks: no
//! plan, no view walk, no job. Where it fails it names the first input that moved and the
//! units that read it ([`Failed`]): the walk that follows starts at those, never at the
//! roots.
//!
//! The invariant: a certificate is given only when a fresh walk would produce the same
//! outputs and say the same.
//!
//! A sans-IO core (rule 14): it reads no file, no clock, no watch and no store; the daemon
//! hands it what is there now ([`Now`], [`Looks`]) and the record's bytes.

// Until `Jobs::begin` asks it and the settle writes it (certificate.md, "State"): the
// record, its codec and its check are whole and tested here first.
#![allow(dead_code)]

use std::fmt;

use blaze_ids::{ContentId, Hasher};

/// The layout of a certificate's bytes ([`Certificate::encode`]). A change of any field's
/// meaning or order takes the next number and a line in [`PREVIOUS`].
pub(super) const LAYOUT: u32 = 1;

/// The first bytes of every certificate.
const MAGIC: &[u8; 8] = b"blzcert\n";

/// The layouts before [`LAYOUT`], each by name: a record of one is refused by that name
/// ([`Undecoded::Previous`]), and the build walks and writes the record anew (rule 26).
pub(super) const PREVIOUS: &[(u32, &str)] =
    &[(0, "the draft of 2026-11-12, whose one group was the view's whole root")];

/// The step a failed certificate is under in `blaze why`.
pub(super) const STEP: &str = "request certificate";

/// What stands for the checkout's own directory in everything a certificate's key is made of
/// ([`relocated`]): no absolute path of a checkout is in a key (store.md, "Content keys").
pub(super) const ROOT: &str = "\u{0}checkout";

/// `text` with the checkout's directory `root` (absolute, no trailing `/`) replaced by
/// [`ROOT`] wherever it is a whole path prefix: at the start of `text` or after a character
/// no path holds in its middle of a word (`=`, `:`, a space, a quote), and followed by `/`
/// or the end. What a request's config is made a shape of, so two checkouts of one content
/// ask for one certificate.
pub(super) fn relocated(text: &str, root: &str) -> String {
    let mut out = String::with_capacity(text.len()); // alloc: once a string of a request's config
    if root.is_empty() {
        out.push_str(text);
        return out;
    }
    let mut rest = text;
    let mut at_start = true;
    while let Some(found) = rest.find(root) {
        let before_ok = match rest[..found].chars().next_back() {
            None => at_start,
            Some(c) => matches!(c, '=' | ':' | ' ' | '"' | '\'' | ','),
        };
        let after = &rest[found + root.len()..];
        let after_ok = after.is_empty() || after.starts_with('/');
        out.push_str(&rest[..found]);
        if before_ok && after_ok {
            out.push_str(ROOT);
        } else {
            out.push_str(root);
        }
        rest = after;
        at_start = false;
    }
    out.push_str(rest);
    out
}

/// The key of the certificate of a request of `shape` (its config as the plan reads it,
/// relocated) on a checkout whose view's root is `root`, served by the blaze `blaze`: content
/// alone.
pub(super) fn key(shape: &ContentId, root: &ContentId, blaze: &str) -> ContentId {
    Hasher::new("blaze v2 serve request certificate")
        .id(shape)
        .id(root)
        .part(blaze.as_bytes())
        .finish()
}

/// The key of the slot of the checkout at `dir` for requests of `shape`: the key of the
/// certificate its last build of that shape left ([`key`]), 32 bytes. The one record of the
/// lane keyed by where a checkout is: it is this machine's alone, never sent to a remote
/// store, and what it names is content-keyed.
pub(super) fn slot(shape: &ContentId, dir: &str) -> ContentId {
    Hasher::new("blaze v2 serve request certificate slot").id(shape).part(dir.as_bytes()).finish()
}

/// Files of the checkout a build read, as one node of the view's tree: a directory by its
/// Merkle id (every file below it, and every name that is not there), or one file by its id.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Group {
    /// From the view's root, `/`-separated; `""` is the root itself. A directory outside the
    /// view (a path dependency) is a group too, by its mount's place ([`relocated`] where it
    /// is named from the checkout, absolute otherwise): its id is read from the tree's mount
    /// when a watch vouches for it and from the disk when none does, the same id either way.
    pub(super) path: String,
    /// The node's id in the view the build read; none: nothing was there, and the build read
    /// that (a negative probe inside the view).
    pub(super) id: Option<ContentId>,
    /// The units that read anything of it, each an index of [`Certificate::units`], sorted.
    pub(super) readers: Vec<u32>,
}

/// An output of the build as it was placed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Output {
    /// Which of the daemon's bases `path` is under ([`Now::bases`] by index: the target
    /// directory, the build directory, the standard library's builds).
    pub(super) base: u8,
    /// From that base, `/`-separated.
    pub(super) path: String,
    /// The store's key of its content.
    pub(super) content: ContentId,
    pub(super) stamp: Stamp,
}

/// What a look at a placed file says: its length and when it last changed. A file replaced
/// by another of the same length at the same nanosecond passes for the placed one, as it
/// does for the store's own placements (`Placement` in the daemon).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Stamp {
    pub(super) len: u64,
    pub(super) changed_ns: u64,
}

/// One request's certificate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Certificate {
    /// The blaze that served the build (its release's version).
    pub(super) blaze: String,
    /// The request's shape, relocated.
    pub(super) shape: ContentId,
    /// The request's config less its environment, relocated.
    pub(super) config: ContentId,
    /// What the request says past its config (verbosity, the goal).
    pub(super) request: ContentId,
    /// How the request's client is shown diagnostics.
    pub(super) drawn: ContentId,
    /// Whether what the build said holds a diagnostic: only then is `drawn` an input.
    pub(super) diagnosed: bool,
    /// The root of the view the build read.
    pub(super) root: ContentId,
    /// The checkout's files the build read, sorted by path, none under another.
    pub(super) groups: Vec<Group>,
    /// Each variable of the request's environment a step read, sorted by name, with the hash
    /// of the value read (none: unset).
    pub(super) env: Vec<(String, Option<ContentId>)>,
    /// Each file outside every view a step read, sorted by path, with the hash of its
    /// content (none: nothing was there, a negative probe). A link on the way to a directory
    /// the build named is here too, by the hash of where it points.
    pub(super) outside: Vec<(String, Option<[u8; 32]>)>,
    /// The outputs, sorted by base then path.
    pub(super) outputs: Vec<Output>,
    /// The units' record keys, in the plan's order: what `blaze why` reads of an answered
    /// request, and what [`Group::readers`] index.
    pub(super) units: Vec<ContentId>,
    /// What the build said, as a build with nothing to do says it, and its `Work`: the
    /// daemon's encoding (deterministic CBOR), opaque here.
    pub(super) said: Vec<u8>,
}

/// Why a certificate's bytes are not read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Undecoded {
    /// Not a certificate at all.
    NotOne,
    /// Of a layout before this one, by its number and name ([`PREVIOUS`]).
    Previous(u32, &'static str),
    /// Of a layout this blaze does not know.
    Unknown(u32),
    /// Cut short or with bytes left over, at this field.
    Malformed(&'static str),
}

impl fmt::Display for Undecoded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Undecoded::NotOne => f.write_str("the record is no request certificate"),
            Undecoded::Previous(n, name) => {
                write!(f, "the request certificate is of layout {n} ({name}); this blaze reads layout {LAYOUT}")
            }
            Undecoded::Unknown(n) => {
                write!(f, "the request certificate is of layout {n}, which this blaze does not know (it reads layout {LAYOUT})")
            }
            Undecoded::Malformed(field) => {
                write!(f, "the request certificate's bytes end or go on wrongly at `{field}`")
            }
        }
    }
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    put_u32(out, u32::try_from(n).expect("a certificate's list is shorter than 2^32"));
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn put_opt(out: &mut Vec<u8>, id: Option<&[u8; 32]>) {
    match id {
        None => out.push(0),
        Some(id) => {
            out.push(1);
            out.extend_from_slice(id);
        }
    }
}

/// A reader of a certificate's bytes: each take names its field for the refusal.
struct Bytes<'a> {
    rest: &'a [u8],
}

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize, field: &'static str) -> Result<&'a [u8], Undecoded> {
        if self.rest.len() < n {
            return Err(Undecoded::Malformed(field));
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Ok(head)
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, Undecoded> {
        Ok(self.take(1, field)?[0])
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, Undecoded> {
        let bytes = self.take(4, field)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self, field: &'static str) -> Result<u64, Undecoded> {
        let bytes = self.take(8, field)?;
        let mut le = [0; 8];
        le.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(le))
    }

    /// A list's length, refused where the bytes left cannot hold that many items of at least
    /// `least` bytes each: no length read off a record reserves more than the record holds.
    fn len(&mut self, least: usize, field: &'static str) -> Result<usize, Undecoded> {
        let n = self.u32(field)? as usize;
        if n.saturating_mul(least.max(1)) > self.rest.len() {
            return Err(Undecoded::Malformed(field));
        }
        Ok(n)
    }

    fn bytes(&mut self, field: &'static str) -> Result<&'a [u8], Undecoded> {
        let n = self.len(1, field)?;
        self.take(n, field)
    }

    fn text(&mut self, field: &'static str) -> Result<String, Undecoded> {
        let bytes = self.bytes(field)?;
        // alloc: a certificate's own string, once a record read
        std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| Undecoded::Malformed(field))
    }

    fn id32(&mut self, field: &'static str) -> Result<[u8; 32], Undecoded> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(32, field)?);
        Ok(id)
    }

    fn id(&mut self, field: &'static str) -> Result<ContentId, Undecoded> {
        self.id32(field).map(ContentId)
    }

    fn opt(&mut self, field: &'static str) -> Result<Option<[u8; 32]>, Undecoded> {
        match self.u8(field)? {
            0 => Ok(None),
            1 => self.id32(field).map(Some),
            _ => Err(Undecoded::Malformed(field)),
        }
    }
}

impl Certificate {
    /// Its bytes: [`MAGIC`], [`LAYOUT`], then each field in the struct's order, every number
    /// little-endian, every list and string under its length. One encoding a value: equal
    /// certificates are equal bytes.
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + self.said.len()); // alloc: the record, once a build that answered
        out.extend_from_slice(MAGIC);
        put_u32(&mut out, LAYOUT);
        put_bytes(&mut out, self.blaze.as_bytes());
        for id in [&self.shape, &self.config, &self.request, &self.drawn] {
            out.extend_from_slice(&id.0);
        }
        out.push(u8::from(self.diagnosed));
        out.extend_from_slice(&self.root.0);
        put_len(&mut out, self.groups.len());
        for group in &self.groups {
            put_bytes(&mut out, group.path.as_bytes());
            put_opt(&mut out, group.id.as_ref().map(|it| &it.0));
            put_len(&mut out, group.readers.len());
            for reader in &group.readers {
                put_u32(&mut out, *reader);
            }
        }
        put_len(&mut out, self.env.len());
        for (name, value) in &self.env {
            put_bytes(&mut out, name.as_bytes());
            put_opt(&mut out, value.as_ref().map(|it| &it.0));
        }
        put_len(&mut out, self.outside.len());
        for (path, hash) in &self.outside {
            put_bytes(&mut out, path.as_bytes());
            put_opt(&mut out, hash.as_ref());
        }
        put_len(&mut out, self.outputs.len());
        for output in &self.outputs {
            out.push(output.base);
            put_bytes(&mut out, output.path.as_bytes());
            out.extend_from_slice(&output.content.0);
            out.extend_from_slice(&output.stamp.len.to_le_bytes());
            out.extend_from_slice(&output.stamp.changed_ns.to_le_bytes());
        }
        put_len(&mut out, self.units.len());
        for unit in &self.units {
            out.extend_from_slice(&unit.0);
        }
        put_bytes(&mut out, &self.said);
        out
    }

    /// The certificate `bytes` are, or why they are not read. A record of a layout before
    /// this one is refused by its name; never read as an empty certificate.
    pub(super) fn decode(bytes: &[u8]) -> Result<Certificate, Undecoded> {
        let mut bytes = Bytes { rest: bytes };
        if bytes.take(MAGIC.len(), "magic").map_err(|_| Undecoded::NotOne)? != MAGIC {
            return Err(Undecoded::NotOne);
        }
        let layout = bytes.u32("layout")?;
        if layout != LAYOUT {
            return Err(match PREVIOUS.iter().find(|(n, _)| *n == layout) {
                Some((n, name)) => Undecoded::Previous(*n, name),
                None => Undecoded::Unknown(layout),
            });
        }
        let blaze = bytes.text("blaze")?;
        let shape = bytes.id("shape")?;
        let config = bytes.id("config")?;
        let request = bytes.id("request")?;
        let drawn = bytes.id("drawn")?;
        let diagnosed = match bytes.u8("diagnosed")? {
            0 => false,
            1 => true,
            _ => return Err(Undecoded::Malformed("diagnosed")),
        };
        let root = bytes.id("root")?;
        let n = bytes.len(9, "groups")?;
        let mut groups = Vec::with_capacity(n); // alloc: the certificate's own, once a record read
        for _ in 0..n {
            let path = bytes.text("group path")?;
            let id = bytes.opt("group id")?.map(ContentId);
            let m = bytes.len(4, "group readers")?;
            let mut readers = Vec::with_capacity(m); // alloc: as above
            for _ in 0..m {
                readers.push(bytes.u32("group reader")?);
            }
            groups.push(Group { path, id, readers });
        }
        let n = bytes.len(5, "env")?;
        let mut env = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            env.push((bytes.text("env name")?, bytes.opt("env value")?.map(ContentId)));
        }
        let n = bytes.len(5, "outside")?;
        let mut outside = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            outside.push((bytes.text("outside path")?, bytes.opt("outside hash")?));
        }
        let n = bytes.len(53, "outputs")?;
        let mut outputs = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            let base = bytes.u8("output base")?;
            let path = bytes.text("output path")?;
            let content = bytes.id("output content")?;
            let stamp = Stamp { len: bytes.u64("output len")?, changed_ns: bytes.u64("output changed")? };
            outputs.push(Output { base, path, content, stamp });
        }
        let n = bytes.len(32, "units")?;
        let mut units = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            units.push(bytes.id("unit")?);
        }
        let said = bytes.bytes("said")?.to_vec(); // alloc: as above
        if !bytes.rest.is_empty() {
            return Err(Undecoded::Malformed("the end"));
        }
        let certificate = Certificate {
            blaze,
            shape,
            config,
            request,
            drawn,
            diagnosed,
            root,
            groups,
            env,
            outside,
            outputs,
            units,
            said,
        };
        let units = certificate.units.len() as u64;
        let in_range = |group: &Group| group.readers.iter().all(|it| u64::from(*it) < units);
        if !certificate.groups.iter().all(in_range) {
            return Err(Undecoded::Malformed("a group's reader is no unit of the certificate"));
        }
        Ok(certificate)
    }
}

/// A request as the certificate's check is asked of it: what the daemon holds by value.
pub(super) struct Now<'a> {
    pub(super) blaze: &'a str,
    pub(super) shape: ContentId,
    pub(super) config: ContentId,
    pub(super) request: ContentId,
    pub(super) drawn: ContentId,
    /// The root of the request's view, as of its checkout's watcher's flush for this request
    /// (`serve::heads` holds the request until that flush is in).
    pub(super) root: ContentId,
    /// The hash of the variable of this name in the request's environment (none: unset).
    pub(super) env: &'a dyn Fn(&str) -> Option<ContentId>,
    /// The request asks for the build's trace (`BLAZE_PROFILE`): a walk is what it asks for,
    /// so it is another request than any a certificate answers.
    pub(super) traced: bool,
}

/// What is there now, as the check looks at it: the view's tree, and the disk outside it.
pub(super) trait Looks {
    /// The id of the node at `path` of the view asked (`blaze_vfs::reader::Pinned::dir_id`
    /// for a directory, the file's id for a file); none: nothing there.
    fn node(&self, path: &str) -> Option<ContentId>;
    /// The hash of the file at `path`, outside every view; none: nothing there.
    fn outside(&self, path: &str) -> Option<[u8; 32]>;
    /// The stamp of the file at `path` under base `base`; none: nothing there.
    fn output(&self, base: u8, path: &str) -> Option<Stamp>;
}

/// Why a certificate is not given: the first input that is not as the build read it. These
/// six and no other (certificate.md, "Why a certificate fails"): not a restart, not another
/// request's walk, not the build's own writes, not a variable no step read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Failed {
    /// Served by another blaze or toolchain.
    Blaze,
    /// The request itself is another: its shape, its config, what it says past them.
    Request(&'static str),
    /// Its client is shown diagnostics another way, and the build said some.
    Drawing,
    /// Files of the checkout: the group at this path, and the units that read it (indices
    /// of [`Certificate::units`]): where the walk starts.
    Group { path: String, readers: Vec<u32> },
    /// A variable a step read.
    Env(String),
    /// A file outside every view a step read; `appeared`: it was not there (a probe).
    Outside { path: String, appeared: bool },
    /// An output is gone or is another file: it is placed again from the store, no unit
    /// walked.
    Output { base: u8, path: String },
}

impl Failed {
    /// The input `blaze why` names.
    pub(super) fn input(&self) -> String {
        match self {
            Failed::Blaze => "the blaze that serves it".to_owned(), // alloc: a reason, once
            Failed::Request(what) => (*what).to_owned(),                 // alloc: as above
            Failed::Drawing => "how diagnostics are shown".to_owned(),   // alloc: as above
            Failed::Group { path, .. } if path.is_empty() => "the checkout's files".to_owned(), // alloc: as above
            Failed::Group { path, .. } => path.clone(), // clone: a reason, once
            Failed::Env(name) => format!("${name}"),
            Failed::Outside { path, .. } => path.clone(), // clone: a reason, once
            Failed::Output { path, .. } => path.clone(),  // clone: a reason, once
        }
    }

    /// The units the walk that follows starts at (indices of [`Certificate::units`]); none:
    /// the failure names no unit, and the walk starts at every unit that reads the request
    /// (a config change) or at none (an output placed again).
    pub(super) fn readers(&self) -> Option<&[u32]> {
        match self {
            Failed::Group { readers, .. } => Some(readers),
            _ => None,
        }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failed::Blaze => f.write_str("the certificate is another blaze's"),
            Failed::Request(what) => write!(f, "{what} is another than the certificate's"),
            Failed::Drawing => f.write_str("its diagnostics were drawn for another client"),
            Failed::Group { path, readers } => {
                let path = if path.is_empty() { "the checkout" } else { path.as_str() };
                write!(f, "{path} changed, which {} unit(s) read", readers.len())
            }
            Failed::Env(name) => write!(f, "the variable {name} changed, which a step read"),
            Failed::Outside { path, appeared: true } => {
                write!(f, "{path} is there now, which a step looked for and did not find")
            }
            Failed::Outside { path, appeared: false } => write!(f, "{path} changed, which a step read"),
            Failed::Output { path, .. } => write!(f, "the output {path} is gone or is another file"),
        }
    }
}

/// A bug planted in the check, for a test's control: with it the check gives a certificate
/// it must not, and the test's oracle says so. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plant {
    None,
    IgnoresGroups,
    IgnoresProbes,
    IgnoresEnv,
    IgnoresOutputs,
    IgnoresRequest,
}

#[cfg(test)]
thread_local! {
    pub(super) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(test))]
fn planted(plant: Plant) -> bool {
    let _ = plant;
    false
}

impl Certificate {
    /// Whether `now` is answered by this: every input of the build is as it read it. The
    /// first that is not is the reason, with the units that read it. Compares before looks,
    /// the view's tree before the disk: a hit costs the compares, one look a file outside the
    /// views and one an output.
    pub(super) fn holds(&self, now: &Now<'_>, looks: &impl Looks) -> Result<(), Failed> {
        if self.blaze != now.blaze {
            return Err(Failed::Blaze);
        }
        if !planted(Plant::IgnoresRequest) {
            if self.shape != now.shape {
                return Err(Failed::Request("the request's command, arguments or plan variables"));
            }
            if self.config != now.config {
                return Err(Failed::Request("the request's configuration"));
            }
            if now.traced {
                return Err(Failed::Request("the request asks for the build's trace, which a walk makes; that"));
            }
            if self.request != now.request {
                return Err(Failed::Request("what the request asks past its configuration"));
            }
            if self.diagnosed && self.drawn != now.drawn {
                return Err(Failed::Drawing);
            }
        }
        // One compare when nothing of the checkout moved; else a node a group, O(groups),
        // never a file each.
        if self.root != now.root && !planted(Plant::IgnoresGroups) {
            for group in &self.groups {
                if looks.node(&group.path) != group.id {
                    return Err(Failed::Group {
                        path: group.path.clone(),       // clone: a reason, once
                        readers: group.readers.clone(), // clone: where the walk starts, once
                    });
                }
            }
        }
        if !planted(Plant::IgnoresEnv) {
            for (name, value) in &self.env {
                if (now.env)(name) != *value {
                    return Err(Failed::Env(name.clone())); // clone: a reason, once
                }
            }
        }
        for (path, hash) in &self.outside {
            if hash.is_none() && planted(Plant::IgnoresProbes) {
                continue;
            }
            if looks.outside(path) != *hash {
                // clone: a reason, once
                return Err(Failed::Outside { path: path.clone(), appeared: hash.is_none() });
            }
        }
        if !planted(Plant::IgnoresOutputs) {
            for output in &self.outputs {
                if looks.output(output.base, &output.path) != Some(output.stamp) {
                    // clone: a reason, once
                    return Err(Failed::Output { base: output.base, path: output.path.clone() });
                }
            }
        }
        Ok(())
    }

    /// Every unit the walk after `failed` visits at most: the readers of the group that
    /// moved and, through `dependents` (a unit's direct dependents, by index), everything
    /// that depends on one. Sorted, each once. None where the failure names no unit.
    pub(super) fn walk_from(
        &self,
        failed: &Failed,
        dependents: &dyn Fn(u32) -> Vec<u32>,
    ) -> Option<Vec<u32>> {
        let mut seen = vec![false; self.units.len()]; // alloc: once a failed certificate
        let mut todo: Vec<u32> = failed.readers()?.to_vec(); // alloc: as above
        let mut out = Vec::new(); // alloc: as above
        // Bounded: each unit is pushed to `out` once, so at most `units` turns do work.
        while let Some(unit) = todo.pop() {
            let Some(slot) = seen.get_mut(unit as usize) else { continue };
            if std::mem::replace(slot, true) {
                continue;
            }
            out.push(unit);
            todo.extend(dependents(unit));
        }
        out.sort_unstable();
        Some(out)
    }
}

#[cfg(test)]
mod tests;
