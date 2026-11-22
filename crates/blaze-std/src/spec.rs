//! The run spec (docs/v2/plan/design/run.md, "What a program is given"): the one value that
//! says everything a started program is given. It is std.md's sandbox spec ("Sandboxes: the
//! one way to run a program"), so there is one such type: [`RunSpec`].
//!
//! | field | holds |
//! |---|---|
//! | `version` | [`VERSION`]; another is refused |
//! | `args` | the command line, the program's name first |
//! | `env` | every environment variable by name and value: nothing else is in the program's [`crate::env::Env`] |
//! | `dir` | the directory it is started in, absolute: the root its file names are beneath, and the one every folder grant is relative to |
//! | `grants` | by name, each a [`Scope`] (a folder with rights, a network endpoint, named variables, or one whole capability: never everything, D104), its [`Provider`] and its [`Layers`] when they differ from the run's |
//! | `provider`, `layers` | the run-wide defaults of both (run.md, "How a run is held") |
//! | `seed` | what a fake provider's world is seeded from |
//! | `limits` | memory (bytes) and time (milliseconds), each or none |
//! | `streams` | where standard input, output and error go |
//!
//! **Strict JSON in, the same bytes out.** [`RunSpec::read`] refuses an unknown field, a field
//! of the wrong kind, a key given twice, a name that is no name and a version it does not know,
//! each with its own [`SpecError`]; [`RunSpec::write`] writes compact JSON, keys sorted, so one
//! value is one text (rule 25), and [`RunSpec::digest`] is the content id of that text: what
//! joins the key of a run or a test result.
//!
//! **How it reaches a program** ([`delivery`], [`read_delivery`]): the parent writes
//! `bstd spec\n` and the JSON to a pipe the child has as file descriptor 3, and closes it; the
//! launcher `#[bstd::main]` makes reads it before `main` (see `crate::entry`). `bstd manifest\n`
//! there instead asks the program for its manifest and runs nothing.
//!
//! Every value is in the allocator its maker names (`A`): the launcher's is the process's
//! heap, taken once by the entry ([`blaze_own::Heap::process`], D102) and handed down.

use core::{alloc::Allocator, fmt};

use blaze_data::{Cursor, Int, Map, ShapeError, Value};
use blaze_ids::ContentId;
use blaze_json::{Duplicates, Limits as JsonLimits, What, WriteOptions};
use blaze_own::{String, Vec};
use blaze_text::{Put, Show, Shown, Writes as _};

use crate::{env::VarName, net::HostPort, path::Path};

/// The spec's format version: the `version` field of every spec.
pub const VERSION: u64 = 1;

/// The line before a spec on file descriptor 3.
pub const SPEC_HEADER: &[u8] = b"bstd spec\n";

/// The line on file descriptor 3 that asks for the program's manifest.
pub const MANIFEST_HEADER: &[u8] = b"bstd manifest\n";

/// The bounds a spec's text is read under: a spec is small; a megabyte is a mistake.
pub const SPEC_LIMITS: JsonLimits =
    JsonLimits { most_bytes: 1 << 20, most_depth: 8, most_number_len: 24, duplicates: Duplicates::Refuse };

/// The domain of [`RunSpec::digest`].
const DIGEST_DOMAIN: &str = "bstd.run-spec.v1";

/// A grant's name: a parameter of `main`, as written (`data`, `api`, `_`): ASCII letters,
/// digits and `_`, not starting with a digit, at most [`GrantName::MOST`] bytes; or a `Many`'s
/// member, `<parameter>.<member>`, each side such a name (crate::grant::is_grant_name).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GrantName<A: Allocator>(String<A>);

impl<A: Allocator + Clone> GrantName<A> {
    pub const MOST: usize = 64;

    /// `raw` as a grant's name, or why it is none.
    pub fn new(raw: &str, alloc: A) -> Result<GrantName<A>, BadName> {
        match crate::grant::is_grant_name(raw) {
            true => Ok(GrantName(String::from_str_in(raw, alloc))),
            false => Err(BadName),
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Whether `raw` is a grant's name, by [`GrantName`]'s rule; a `const fn`, so the manifest
/// checks a parameter's name as the crate compiles by the same rule.
pub const fn is_name(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 || bytes[0].is_ascii_digit() {
        return false;
    }
    let mut at = 0;
    while at < bytes.len() {
        if !(bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
            return false;
        }
        at += 1;
    }
    true
}

/// A name that is no grant's name ([`GrantName`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BadName;

impl fmt::Display for BadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a grant's name is a parameter's: ASCII letters, digits and `_`, not starting with a digit, at most 64 bytes")
    }
}

/// What a grant reaches.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Scope<A: Allocator> {
    /// A folder beneath the spec's `dir` (a bstd [`Path`]: relative, no `..`), read only or
    /// read and write.
    Dir { path: String<A>, write: bool },
    /// One network endpoint, `host:port` ([`HostPort`]): connected to, or listened on.
    Net { at: String<A>, listen: bool },
    /// The environment variables of these names, sorted, each once.
    Env { names: Vec<String<A>, A> },
    /// One root capability whole, by its name in [`Offers::NAMES`] (`fs`, `rng`, `host`, ...)
    /// or `crate::grant::BY_MACHINE` (`gpu`); with a `device`, only that device of it (a
    /// capability of `crate::grant::ONE_DEVICE`: `audio`, `audio_in`, `camera`, ...).
    Whole { capability: String<A>, device: Option<String<A>> },
}

impl<A: Allocator> Scope<A> {
    /// Its kind's name, as its key in the spec's JSON.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Scope::Dir { .. } => "dir",
            Scope::Net { .. } => "net",
            Scope::Env { .. } => "env",
            Scope::Whole { .. } => "whole",
        }
    }
}

/// What answers a grant's effects (run.md, "How a run is held").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Provider {
    /// The machine: `blaze run`.
    Real,
    /// The simulated world the tests use, seeded by the spec's `seed`: `--fake`.
    Fake,
    /// Every effect answered from a record: `blaze replay`.
    Replay,
}

impl Provider {
    const NAMES: [&'static str; 3] = ["real", "fake", "replay"];

    pub fn name(self) -> &'static str {
        Provider::NAMES[self as usize]
    }

    fn of(at: usize) -> Provider {
        match at {
            0 => Provider::Real,
            1 => Provider::Fake,
            _ => Provider::Replay,
        }
    }
}

/// What is wrapped around a grant's provider: a set of [`Layer`]s.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Layers(u8);

/// One layer around a provider (run.md, "How a run is held").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Layer {
    /// Every effect and its answer written down: `--record`.
    Record = 0,
    /// Writes, sends and spawns kept in an overlay the program sees and the world does not:
    /// `--dry`.
    Overlay = 1,
    /// Each new kind and scope of reach asked about first: `--ask`.
    Ask = 2,
}

impl Layer {
    const ALL: [Layer; 3] = [Layer::Record, Layer::Overlay, Layer::Ask];
    const NAMES: [&'static str; 3] = ["record", "overlay", "ask"];

    pub fn name(self) -> &'static str {
        Layer::NAMES[self as usize]
    }
}

impl Layers {
    pub const NONE: Layers = Layers(0);

    pub fn with(self, layer: Layer) -> Layers {
        Layers(self.0 | 1 << layer as u8)
    }

    pub fn has(self, layer: Layer) -> bool {
        self.0 & 1 << layer as u8 != 0
    }

    /// Each layer in the set, in [`Layer`]'s order: the order they are written in.
    pub fn each(self) -> impl Iterator<Item = Layer> {
        Layer::ALL.into_iter().filter(move |layer| self.has(*layer))
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// One grant: its name, its scope, and its provider and layers where they are not the run's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Granted<A: Allocator> {
    pub name: GrantName<A>,
    pub scope: Scope<A>,
    pub provider: Option<Provider>,
    pub layers: Option<Layers>,
}

/// The run's limits; `None` is none.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RunLimits {
    /// The most bytes of memory the process may hold.
    pub memory: Option<u64>,
    /// The most milliseconds `main` may run: its root context's deadline.
    pub time_ms: Option<u64>,
}

/// Where one stream comes from or goes to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Stream<A: Allocator> {
    /// The parent's own stream.
    Inherit,
    /// Nothing: an empty input, output thrown away.
    Null,
    /// A file beneath the spec's `dir`.
    File(String<A>),
}

/// Where standard input, output and error go: the parent sets them up before the program
/// starts; the spec says what it did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Streams<A: Allocator> {
    pub stdin: Stream<A>,
    pub stdout: Stream<A>,
    pub stderr: Stream<A>,
}

/// Everything a started program is given (the module's doc).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunSpec<A: Allocator> {
    pub args: Vec<String<A>, A>,
    /// Sorted by name, each name once.
    pub env: Vec<(String<A>, String<A>), A>,
    pub dir: String<A>,
    /// Sorted by name, each name once.
    pub grants: Vec<Granted<A>, A>,
    pub provider: Provider,
    pub layers: Layers,
    pub seed: u64,
    pub limits: RunLimits,
    pub streams: Streams<A>,
}

/// Why a spec was refused: each its own, each naming where.
pub enum SpecError<A: Allocator> {
    /// File descriptor 3 held neither [`SPEC_HEADER`] nor [`MANIFEST_HEADER`] first.
    NoHeader,
    /// The text is no JSON, or passes [`SPEC_LIMITS`].
    Json { error: blaze_json::Error, line: usize, col: usize },
    /// A key given twice in one object: a grant, a variable or a field.
    Duplicate { key: String<A>, line: usize, col: usize },
    /// The JSON does not fit the spec: an unknown field, a missing one, a wrong kind, a name
    /// that is no name; the path and why.
    Shape(ShapeError<A>),
    /// A version this bstd does not read.
    Version { found: u64 },
}

impl<A: Allocator> From<ShapeError<A>> for SpecError<A> {
    fn from(error: ShapeError<A>) -> SpecError<A> {
        SpecError::Shape(error)
    }
}

impl<A: Allocator> Show for SpecError<A> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.str("bstd: the run spec: ");
        match self {
            SpecError::NoHeader => {
                out.str("file descriptor 3 is open but does not start with `bstd spec` or `bstd manifest`: start the \
                         program with `blaze run`, or with file descriptor 3 closed");
            }
            SpecError::Json { error, line, col } => {
                out.str("line ").int(*line).str(", column ").int(*col).str(": ");
                blaze_text::PutDisplay::put_display(out, error);
            }
            SpecError::Duplicate { key, line, col } => {
                out.str("`").str(key.as_str()).str("` is given twice in one object (line ").int(*line).str(", column ");
                out.int(*col).str("): a grant, a variable and a field are each given once");
            }
            SpecError::Shape(error) => {
                blaze_text::PutDisplay::put_display(out, error);
            }
            SpecError::Version { found } => {
                out.str("version ").int(*found).str(" is not one this program reads (it reads ").int(VERSION).str(")");
            }
        }
    }
}

impl<A: Allocator> fmt::Display for SpecError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl<A: Allocator> fmt::Debug for SpecError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl<A: Allocator> core::error::Error for SpecError<A> {}

/// What file descriptor 3 asked of the program.
pub enum Asked<A: Allocator> {
    /// Run, given this.
    Run(RunSpec<A>),
    /// Print the manifest and run nothing.
    Manifest,
}

/// The bytes a parent writes to its child's file descriptor 3: [`SPEC_HEADER`], then the spec.
pub fn delivery<A: Allocator + Clone>(spec: &RunSpec<A>, alloc: A) -> Vec<u8, A> {
    let text = spec.write(alloc.clone());
    let mut bytes = Vec::with_capacity_in(SPEC_HEADER.len() + text.len(), alloc);
    bytes.extend_from_slice(SPEC_HEADER);
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// What `bytes`, all file descriptor 3 held, ask.
pub fn read_delivery<A: Allocator + Clone>(bytes: &[u8], alloc: A) -> Result<Asked<A>, SpecError<A>> {
    if bytes == MANIFEST_HEADER {
        return Ok(Asked::Manifest);
    }
    match bytes.strip_prefix(SPEC_HEADER) {
        Some(json) => RunSpec::read(json, alloc).map(Asked::Run),
        None => Err(SpecError::NoHeader),
    }
}

impl<A: Allocator + Clone> RunSpec<A> {
    /// The spec of a program started with none: its command line, its environment and its
    /// directory as they are, no grants, the real provider, no layers, no limits, its parent's
    /// streams.
    pub fn defaults(args: Vec<String<A>, A>, mut env: Vec<(String<A>, String<A>), A>, dir: String<A>, alloc: A) -> RunSpec<A> {
        env.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        RunSpec {
            args,
            env,
            dir,
            grants: Vec::new_in(alloc),
            provider: Provider::Real,
            layers: Layers::NONE,
            seed: 0,
            limits: RunLimits::default(),
            streams: Streams { stdin: Stream::Inherit, stdout: Stream::Inherit, stderr: Stream::Inherit },
        }
    }

    /// The grant named `name`.
    pub fn grant(&self, name: &str) -> Option<&Granted<A>> {
        self.grants.iter().find(|granted| granted.name.as_str() == name)
    }

    /// `granted` added, keeping the grants sorted; one of the same name is replaced.
    pub fn put_grant(&mut self, granted: Granted<A>) {
        match self.grants.binary_search_by(|it| it.name.as_str().cmp(granted.name.as_str())) {
            Ok(at) => self.grants[at] = granted,
            Err(at) => self.grants.insert_shift(at, granted),
        }
    }

    /// The spec in `bytes`, strictly (the module's doc).
    pub fn read(bytes: &[u8], alloc: A) -> Result<RunSpec<A>, SpecError<A>> {
        let value = blaze_json::read_in(bytes, SPEC_LIMITS, alloc.clone()).map_err(|error| {
            let (line, col) = error.line_col(bytes);
            match error.what {
                What::DuplicateKey => SpecError::Duplicate { key: key_at(bytes, error.at, alloc.clone()), line, col },
                _ => SpecError::Json { error, line, col },
            }
        })?;
        let root = Cursor::root(&value, alloc.clone());
        root.fields(|f| {
            let version: u64 = f.take("version")?;
            if version != VERSION {
                return Err(SpecError::Version { found: version });
            }
            let args = f.take::<Vec<String<A>, A>>("args")?;
            let env = read_env(&f.cursor("env")?, alloc.clone())?;
            let dir_at = f.cursor("dir")?;
            let dir: String<A> = dir_at.get()?;
            if !dir.as_str().starts_with('/') {
                return Err(dir_at.invalid("directory", &"the directory a program starts in is absolute").into());
            }
            let grants = read_grants(&f.cursor("grants")?, alloc.clone())?;
            let provider = read_provider(&f.cursor("provider")?)?;
            let layers = read_layers(&f.cursor("layers")?)?;
            let seed: u64 = f.take("seed")?;
            let limits = f.cursor("limits")?.fields(|l| {
                Ok::<_, ShapeError<A>>(RunLimits { memory: l.take_opt("memory")?, time_ms: l.take_opt("time_ms")? })
            })?;
            let streams = f.cursor("streams")?.fields(|s| {
                Ok::<_, ShapeError<A>>(Streams {
                    stdin: read_stream(&s.cursor("stdin")?)?,
                    stdout: read_stream(&s.cursor("stdout")?)?,
                    stderr: read_stream(&s.cursor("stderr")?)?,
                })
            })?;
            Ok(RunSpec { args, env, dir, grants, provider, layers, seed, limits, streams })
        })
    }

    /// The spec as compact JSON, keys sorted: one value, one text.
    pub fn write(&self, alloc: A) -> String<A> {
        let value = self.to_value(alloc.clone());
        match blaze_json::to_string_in(&value, WriteOptions::COMPACT, alloc) {
            Ok(text) => text,
            // The value holds no bytes and no float, the only things JSON cannot say.
            Err(_) => unreachable!("a run spec's value is text, integers, booleans, lists and maps"),
        }
    }

    /// The content id of [`RunSpec::write`]'s text.
    pub fn digest(&self, alloc: A) -> ContentId {
        ContentId::of(DIGEST_DOMAIN, self.write(alloc).as_bytes())
    }

    fn to_value(&self, a: A) -> Value<A> {
        let mut root = Map::new_in(a.clone());
        put(&mut root, "version", Value::Int(Int::from_u64(VERSION)), &a);
        let mut args = Vec::with_capacity_in(self.args.len(), a.clone());
        for arg in self.args.iter() {
            args.push(text(arg.as_str(), &a));
        }
        put(&mut root, "args", Value::List(args), &a);
        let mut env = Map::new_in(a.clone());
        for (name, value) in self.env.iter() {
            put_owned(&mut env, name.as_str(), text(value.as_str(), &a), &a);
        }
        put(&mut root, "env", Value::Map(env), &a);
        put(&mut root, "dir", text(self.dir.as_str(), &a), &a);
        let mut grants = Map::new_in(a.clone());
        for granted in self.grants.iter() {
            put_owned(&mut grants, granted.name.as_str(), granted_value(granted, &a), &a);
        }
        put(&mut root, "grants", Value::Map(grants), &a);
        put(&mut root, "provider", text(self.provider.name(), &a), &a);
        put(&mut root, "layers", layers_value(self.layers, &a), &a);
        put(&mut root, "seed", Value::Int(Int::from_u64(self.seed)), &a);
        let mut limits = Map::new_in(a.clone());
        put(&mut limits, "memory", opt_int(self.limits.memory), &a);
        put(&mut limits, "time_ms", opt_int(self.limits.time_ms), &a);
        put(&mut root, "limits", Value::Map(limits), &a);
        let mut streams = Map::new_in(a.clone());
        put(&mut streams, "stdin", stream_value(&self.streams.stdin, &a), &a);
        put(&mut streams, "stdout", stream_value(&self.streams.stdout, &a), &a);
        put(&mut streams, "stderr", stream_value(&self.streams.stderr, &a), &a);
        put(&mut root, "streams", Value::Map(streams), &a);
        Value::Map(root)
    }
}

/// The key that starts at `at` in `bytes` (a duplicate's, where the reader stopped): the text
/// between its quotes, unescaped bytes only; empty when it is not there.
fn key_at<A: Allocator + Clone>(bytes: &[u8], at: usize, alloc: A) -> String<A> {
    let rest = bytes.get(at + 1..).unwrap_or(&[]);
    let end = rest.iter().position(|b| *b == b'"' || *b == b'\\').unwrap_or(0);
    String::from_str_in(core::str::from_utf8(&rest[..end]).unwrap_or(""), alloc)
}

fn text<A: Allocator + Clone>(raw: &str, a: &A) -> Value<A> {
    Value::Text(String::from_str_in(raw, a.clone()))
}

fn opt_int<A: Allocator>(n: Option<u64>) -> Value<A> {
    match n {
        Some(n) => Value::Int(Int::from_u64(n)),
        None => Value::Null,
    }
}

fn put<A: Allocator + Clone>(map: &mut Map<A>, key: &'static str, value: Value<A>, a: &A) {
    put_owned(map, key, value, a);
}

fn put_owned<A: Allocator + Clone>(map: &mut Map<A>, key: &str, value: Value<A>, a: &A) {
    map.insert(String::from_str_in(key, a.clone()), value);
}

fn layers_value<A: Allocator + Clone>(layers: Layers, a: &A) -> Value<A> {
    let mut list = Vec::new_in(a.clone());
    for layer in layers.each() {
        list.push(text(layer.name(), a));
    }
    Value::List(list)
}

fn stream_value<A: Allocator + Clone>(stream: &Stream<A>, a: &A) -> Value<A> {
    match stream {
        Stream::Inherit => text("inherit", a),
        Stream::Null => text("null", a),
        Stream::File(path) => {
            let mut map = Map::new_in(a.clone());
            put(&mut map, "file", text(path.as_str(), a), a);
            Value::Map(map)
        }
    }
}

fn granted_value<A: Allocator + Clone>(granted: &Granted<A>, a: &A) -> Value<A> {
    let mut map = Map::new_in(a.clone());
    let scope = match &granted.scope {
        Scope::Dir { path, write } => {
            let mut dir = Map::new_in(a.clone());
            put(&mut dir, "path", text(path.as_str(), a), a);
            put(&mut dir, "write", Value::Bool(*write), a);
            Value::Map(dir)
        }
        Scope::Net { at, listen } => {
            let mut net = Map::new_in(a.clone());
            put(&mut net, "at", text(at.as_str(), a), a);
            put(&mut net, "listen", Value::Bool(*listen), a);
            Value::Map(net)
        }
        Scope::Env { names } => {
            let mut list = Vec::with_capacity_in(names.len(), a.clone());
            for name in names.iter() {
                list.push(text(name.as_str(), a));
            }
            Value::List(list)
        }
        Scope::Whole { capability, device: None } => text(capability.as_str(), a),
        Scope::Whole { capability, device: Some(device) } => {
            let mut one = Map::new_in(a.clone());
            put(&mut one, "capability", text(capability.as_str(), a), a);
            put(&mut one, "device", text(device.as_str(), a), a);
            Value::Map(one)
        }
    };
    put(&mut map, granted.scope.kind_name(), scope, a);
    if let Some(provider) = granted.provider {
        put(&mut map, "provider", text(provider.name(), a), a);
    }
    if let Some(layers) = granted.layers {
        put(&mut map, "layers", layers_value(layers, a), a);
    }
    Value::Map(map)
}

fn read_env<A: Allocator + Clone>(at: &Cursor<'_, '_, A>, alloc: A) -> Result<Vec<(String<A>, String<A>), A>, SpecError<A>> {
    let mut env = Vec::new_in(alloc.clone());
    at.entries(|name, value| {
        if let Err(why) = VarName::new(name) {
            return Err(SpecError::Shape(value.invalid("variable name", &why)));
        }
        env.push((String::from_str_in(name, alloc.clone()), value.get::<String<A>>()?));
        Ok::<_, SpecError<A>>(())
    })?;
    env.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    Ok(env)
}

fn read_provider<A: Allocator + Clone>(at: &Cursor<'_, '_, A>) -> Result<Provider, ShapeError<A>> {
    at.one_of(&Provider::NAMES).map(Provider::of)
}

fn read_layers<A: Allocator + Clone>(at: &Cursor<'_, '_, A>) -> Result<Layers, ShapeError<A>> {
    let mut layers = Layers::NONE;
    at.each(|item| {
        let layer = Layer::ALL[item.one_of(&Layer::NAMES)?];
        if layers.has(layer) {
            return Err(item.invalid("layers", &"a layer is named once"));
        }
        layers = layers.with(layer);
        Ok(())
    })?;
    Ok(layers)
}

fn read_stream<A: Allocator + Clone>(at: &Cursor<'_, '_, A>) -> Result<Stream<A>, ShapeError<A>> {
    match at.value() {
        Value::Map(_) => at.fields(|f| {
            let file = f.cursor("file")?;
            let path: String<A> = file.get()?;
            match Path::new(path.as_str()) {
                Ok(_) => Ok(Stream::File(path)),
                Err(why) => Err(file.invalid("path", &why)),
            }
        }),
        _ => match at.one_of(&["inherit", "null"])? {
            0 => Ok(Stream::Inherit),
            _ => Ok(Stream::Null),
        },
    }
}

/// The scope keys a grant object may hold, one of them.
const SCOPES: [&str; 4] = ["dir", "net", "env", "whole"];

fn read_grants<A: Allocator + Clone>(at: &Cursor<'_, '_, A>, alloc: A) -> Result<Vec<Granted<A>, A>, SpecError<A>> {
    let mut grants: Vec<Granted<A>, A> = Vec::new_in(alloc.clone());
    at.entries(|name, value| {
        let name = match GrantName::new(name, alloc.clone()) {
            Ok(name) => name,
            Err(why) => return Err(SpecError::Shape(value.invalid("grant name", &why))),
        };
        let granted = value.fields(|f| {
            let mut scope = None;
            for key in SCOPES {
                if let Some(c) = f.cursor_opt(key)? {
                    if scope.is_some() {
                        return Err(c.invalid("grant", &"a grant has one scope: one of `dir`, `net`, `env`, `whole`"));
                    }
                    scope = Some(read_scope(key, &c, alloc.clone())?);
                }
            }
            let Some(scope) = scope else {
                return Err(value.invalid("grant", &"a grant has a scope: one of `dir`, `net`, `env`, `whole` (a whole world cannot be granted: D104)"));
            };
            let provider = match f.cursor_opt("provider")? {
                Some(c) => Some(read_provider(&c)?),
                None => None,
            };
            let layers = match f.cursor_opt("layers")? {
                Some(c) => Some(read_layers(&c)?),
                None => None,
            };
            // clone: a grant's name, a few bytes.
            Ok::<_, ShapeError<A>>(Granted { name: name.clone(), scope, provider, layers })
        })?;
        grants.push(granted);
        Ok::<_, SpecError<A>>(())
    })?;
    grants.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    Ok(grants)
}

fn read_scope<A: Allocator + Clone>(key: &str, at: &Cursor<'_, '_, A>, alloc: A) -> Result<Scope<A>, ShapeError<A>> {
    match key {
        "dir" => at.fields(|f| {
            let path_at = f.cursor("path")?;
            let path: String<A> = path_at.get()?;
            if path.as_str() != crate::grant::START_FOLDER
                && let Err(why) = Path::new(path.as_str())
            {
                return Err(path_at.invalid("folder", &why));
            }
            Ok(Scope::Dir { path, write: f.take("write")? })
        }),
        "net" => at.fields(|f| {
            let at_at = f.cursor("at")?;
            let endpoint: String<A> = at_at.get()?;
            if let Err(why) = HostPort::parse(endpoint.as_str()) {
                return Err(at_at.invalid("host:port", &why));
            }
            Ok(Scope::Net { at: endpoint, listen: f.take("listen")? })
        }),
        "env" => {
            let mut names: Vec<String<A>, A> = Vec::new_in(alloc);
            at.each(|item| {
                let name: String<A> = item.get()?;
                if let Err(why) = VarName::new(name.as_str()) {
                    return Err(item.invalid("variable name", &why));
                }
                if names.iter().any(|it| *it == name) {
                    return Err(item.invalid("variable name", &"a variable is named once in a grant"));
                }
                names.push(name);
                Ok(())
            })?;
            names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            Ok(Scope::Env { names })
        }
        _ if matches!(at.value(), Value::Map(_)) => at.fields(|f| {
            let capability_at = f.cursor("capability")?;
            let capability: String<A> = capability_at.get()?;
            if !crate::grant::narrows_to_device(capability.as_str()) {
                return Err(capability_at.invalid("capability", &"not one a grant narrows to a device (`bstd::grant::ONE_DEVICE`)"));
            }
            let device_at = f.cursor("device")?;
            let device: String<A> = device_at.get()?;
            if crate::name::refused(device.as_str()).is_some() {
                return Err(device_at.invalid("device name", &"1 to 255 bytes, no control byte, no space at either end"));
            }
            Ok(Scope::Whole { capability, device: Some(device) })
        }),
        _ => {
            let capability: String<A> = at.get()?;
            match crate::grant::is_whole_capability(capability.as_str()) {
                true => Ok(Scope::Whole { capability, device: None }),
                false => Err(at.invalid("capability", &"not one of bstd's capabilities (`Offers::NAMES`, `grant::BY_MACHINE`)")),
            }
        }
    }
}

/// Why a `--grant name=value` was refused: blaze-cli prints it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GrantArgError {
    /// No `=`.
    NoEquals,
    /// `main` takes no parameter of that name.
    NoParameter,
    /// The parameter fills itself: it takes no grant.
    FillsItself,
    /// The value does not read as the parameter's kind wants; what it wants.
    Value { wants: &'static str },
}

impl fmt::Display for GrantArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrantArgError::NoEquals => f.write_str("a grant is `--grant <parameter>=<scope>`"),
            GrantArgError::NoParameter => f.write_str("`main` takes no parameter of that name"),
            GrantArgError::FillsItself => f.write_str("that parameter fills itself and takes no grant"),
            GrantArgError::Value { wants } => write!(f, "the scope is not {wants}"),
        }
    }
}

impl core::error::Error for GrantArgError {}

/// `arg`, the text after `--grant` (`data=fixtures`, `api=api.example.com:443`), read as a
/// grant for the program whose manifest is `manifest`: the parameter's kind says how the
/// value reads and which rights a folder gets (the ones the parameter asks for). The one home
/// of the grant syntax; blaze-cli calls it for each `--grant`.
pub fn grant_arg<A: Allocator + Clone>(
    manifest: &crate::grant::Manifest,
    arg: &str,
    alloc: A,
) -> Result<Granted<A>, GrantArgError> {
    let (name, value) = arg.split_once('=').ok_or(GrantArgError::NoEquals)?;
    // A `Many`'s member (`inputs.a`) is taken by its parameter (`inputs`).
    let param = manifest.taker(name).ok_or(GrantArgError::NoParameter)?;
    let name = GrantName::new(name, alloc.clone()).map_err(|_| GrantArgError::NoParameter)?;
    // A `OneOf`'s value names its alternative: `dir:..`, `read:..`, `net:..`, `env:..`, `fs:whole`.
    let (kind, value) = match param.form {
        crate::grant::Form::OneOf(kinds) => crate::grant::compose_alternative(kinds, value)
            .ok_or(GrantArgError::Value { wants: "<alternative>:<scope> (dir, read, net, env, or a capability's name)" })?,
        crate::grant::Form::One | crate::grant::Form::Optional | crate::grant::Form::Many => (param.kind, value),
    };
    let scope = crate::grant::compose_scope(kind, param.narrow, value, alloc)?;
    Ok(Granted { name, scope, provider: None, layers: None })
}
