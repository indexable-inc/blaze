//! What may be published to the global cache or fetched from it.
//!
//! A store [`Key`] is a hash: nothing in it says what it was made from. So the rule is
//! checked where a key is made, from what it was made from: a [`QueryMade`], a [`RunMade`],
//! a [`FunctionMade`] or [`DevFunctionMade`], a side table of a public function, or a
//! [`DerivedMade`] from a public front-end value. Each check makes the key again with the
//! function its owner makes it with (`Key::of`, `blaze_usercode::index`,
//! `blaze_codegen::query::keys`, `blaze_diagnostics::cache`, `hir_ty::consteval::deps`), so
//! what is checked is what the key is a hash of, and then
//! checks every input's [`Origin`]. A dependency's front-end image ([`ImageMade`]) is the one
//! key not made again here: it is a hash of the crate graph and files only the front end's
//! database holds, so what it covers comes from the walk that makes it, and is checked the
//! same way. What passes is a [`Public`]: the client fetches and the builders publish only
//! those.
//!
//! Public is:
//! - a crates.io crate version, named by the SHA-256 of its `.crate` the lock file gives and
//!   the download was checked against ([`Origin::CratesIo`]);
//! - the pinned toolchain's files: std, core, alloc and the rest of its sysroot
//!   ([`Origin::Toolchain`]);
//! - the system image of blaze's builders, which build scripts read headers and a C compiler
//!   from ([`Origin::System`]);
//! - what public runs wrote: the `OUT_DIR`s of build scripts of public crates
//!   ([`Origin::Outputs`]).
//!
//! Never public: a workspace member, a path dependency, a git dependency, a registry other
//! than crates.io, a variable of the machine's environment a run read with a value, a run
//! that read the whole environment or what can't be traced, a test, and code compiled for
//! more than the target's baseline. And whatever the inputs, a value holding a [`Secrets`]
//! entry is never published.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    path::{Component, Path, PathBuf},
};

use blaze_cas::{Key, Kind};
use blaze_codegen::{Backend, Instance, InstanceKey, query::keys, settings::Spec, summary::Imports};
use blaze_core::{Traced, Untraceable};
use blaze_diagnostics::cache::{self, Inputs};
use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_usercode::{Given, RunKey, index, trace};
use hir_ty_borrowck::Mode;
use memchr::memmem;

use crate::sign::Publisher;

/// Where an input of a key comes from.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Origin {
    /// A crates.io crate version: the SHA-256 of its `.crate`, as the lock file names it.
    CratesIo { name: String, version: String, checksum: [u8; 32] },
    /// The pinned toolchain, by its code identity (`blaze_codegen::settings::Toolchain::id`).
    Toolchain(ContentId),
    /// A system image of blaze's builders, by its content ID.
    System(ContentId),
    /// The `OUT_DIR`s of build scripts of public crates.
    Outputs,
    /// A member of the user's workspace.
    Workspace,
    /// A path dependency outside the workspace.
    Path,
    Git { url: String },
    /// A registry other than crates.io.
    Registry { index: String },
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::CratesIo { name, version, .. } => write!(f, "{name} {version} from crates.io"),
            Origin::Toolchain(id) => write!(f, "the toolchain {:x}", id),
            Origin::System(id) => write!(f, "the system image {:x}", id),
            Origin::Outputs => f.write_str("the outputs of public build scripts"),
            Origin::Workspace => f.write_str("a workspace member"),
            Origin::Path => f.write_str("a path dependency"),
            Origin::Git { url } => write!(f, "the git dependency {url}"),
            Origin::Registry { index } => write!(f, "the registry {index}"),
        }
    }
}

/// A directory of this machine and where what is under it comes from. The longest root a
/// path is under says where it comes from, so a private directory inside a public one (or
/// the reverse) is named as what it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Root {
    pub path: PathBuf,
    pub origin: Origin,
}

/// The rule of one machine: what it pins and where its public sources are.
#[derive(Clone, Debug)]
pub struct Rule {
    toolchain: ContentId,
    /// The blaze that reads and writes dependency images: its own code with the nightly it
    /// compiles against, as their keys name it.
    build: ContentId,
    systems: Vec<ContentId>,
    roots: Vec<Root>,
    /// Each root by the hash of its components ([`component_hash`]) and its depth, sorted:
    /// `(hash, depth, index in roots)`.
    by_hash: Vec<(u64, u32, u32)>,
    /// Bit `d` (63 for 63 and deeper): some root has `d` components.
    depths: u64,
    public_env: Vec<OsString>,
}

/// A key proven public: by a [`Rule`] check, or by a signed bundle manifest that lists it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Public {
    key: Key,
    kind: Kind,
}

impl Public {
    pub fn key(&self) -> Key {
        self.key
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// A key a pinned publisher listed in a verified manifest.
    pub(crate) fn listed(key: Key, kind: Kind) -> Public {
        Public { key, kind }
    }
}

/// Why a key is not public.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Private {
    pub key: Key,
    pub why: String,
}

impl fmt::Display for Private {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x} is not public: {}", self.key.0, self.why)
    }
}

/// A query's value: `Key::of(schema, producer, inputs)`. `producer` is blaze's own code.
#[derive(Clone, Copy, Debug)]
pub struct QueryMade<'a> {
    pub schema: SchemaHash,
    pub producer: ContentId,
    pub inputs: &'a [ContentId],
    /// Where each of `inputs` comes from, in its order.
    pub origins: &'a [Origin],
}

/// A build script's or proc macro's run.
#[derive(Clone, Copy, Debug)]
pub struct RunMade<'a> {
    pub run: RunKey,
    /// What it was given: [`Given::finish`] of it is `run.given`.
    pub given: &'a Given,
    /// The crate of the build script or proc macro.
    pub code: &'a Origin,
    /// The crate it runs for: the package of a build script, the crate a macro expands in.
    pub caller: &'a Origin,
    pub traced: &'a Traced,
}

/// Which of a run's values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunPart {
    /// Its result, under `index::result_key`.
    Result,
    /// Slot `n` of the chain of its shapes, under `index::slot`.
    Shape(u32),
}

/// A function instance and the crates its code and type arguments come from.
#[derive(Clone, Debug)]
pub struct FnSource {
    pub instance: Instance,
    /// The crate of the item its code hash is of.
    pub code: Origin,
    /// The crates of the facts of its type arguments: the types' crates, the toolchain for
    /// primitives.
    pub args: Vec<Origin>,
}

/// An LLVM-optimized function as the release driver keeps it: under
/// `keys::release(imports.blob_key(stored(instance, built_in)))`.
#[derive(Clone, Copy, Debug)]
pub struct FunctionMade<'a> {
    /// The settings its instance's `Settings` are of.
    pub spec: &'a Spec,
    pub function: &'a FnSource,
    /// The bodies its code builds in, in the driver's order.
    pub built_in: &'a [FnSource],
    pub imports: &'a Imports,
    /// Each of `imports.imported`, in its order.
    pub imported: &'a [FnSource],
}

/// A dependency crate's image key and what it covers, as `hir_def::deps` made it. The key is a
/// hash of the crate graph and its files, which only the front end's database holds, so the
/// walk that makes the key gives these with it (`hir_def::deps::KeyInputs`) and the driver
/// passes them on. Images are kept as [`Kind::Query`] values.
#[derive(Clone, Copy, Debug)]
pub struct ImageMade<'a> {
    /// The crate's key (`hir_def::deps::keys`).
    pub key: ContentId,
    /// The blaze build the key was made by.
    pub build: ContentId,
    /// Where the crate and each crate of its dependency closure come from, the crate first.
    pub crates: &'a [Origin],
    /// The `OUT_DIR` of each of `crates` that has one.
    pub out_dirs: &'a [&'a Path],
    /// The names the crate read besides its key's inputs (its `Reads`), which its slots keep.
    pub reads: &'a [ImageRead<'a>],
}

/// A name a crate's resolution read: by an `env!`, or by one of its proc-macro expansions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageRead<'a> {
    File(&'a Path),
    /// A directory's listing.
    Dir(&'a Path),
    /// A variable of the process or of the shell an expansion ran in.
    Env(&'a OsStr),
    /// What an expansion read that can't be traced.
    Untraceable(Untraceable),
}

/// Which key of a crate's image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImagePart {
    /// The image under the crate's key alone: a crate that read nothing besides its key's
    /// inputs.
    Image,
    /// A slot of the key, keeping the names the crate read (`hir_def::deps::reads::slot`).
    Slot(ContentId),
    /// The image filed by those names and their values (`hir_def::deps::reads::image_key`).
    Filed(ContentId),
}

/// A dev function (Cranelift's or copy-and-patch's) as the dev driver keeps it: under
/// `keys::template(stored(instance, built_in))`. It has no import decisions: those are
/// release code's.
#[derive(Clone, Copy, Debug)]
pub struct DevFunctionMade<'a> {
    /// The settings its instance's `Settings` are of.
    pub spec: &'a Spec,
    pub function: &'a FnSource,
    /// The bodies its code builds in, in the driver's order.
    pub built_in: &'a [FnSource],
}

/// A dependency's front-end result, keyed by its owner from a public base key (a crate's
/// image key, or its check key) and the further inputs here.
#[derive(Clone, Copy, Debug)]
pub enum DerivedMade<'a> {
    /// The bodies part of a crate's check (`blaze_diagnostics::cache::bodies_key`), from its
    /// image: `deps` are its dependencies' check keys, in its dependency list's order, each
    /// proven public. Its key is the crate's check key.
    Bodies { deps: &'a [Public], borrowck: Mode },
    /// A crate's check outcome (`blaze_diagnostics::cache::key`), from its check key: the
    /// lint flags cargo gives and the borrow checker's mode.
    Outcome(Inputs<'a>),
    /// The value of a constant of the crate (`hir_ty::consteval::deps::key`), from its check
    /// key: `item` is the constant's stable key (`hir_def::deps::const_key`).
    Const { item: ContentId },
}

/// The hash of a path's components up to and with `component`, from `hash`, theirs before
/// it: equal for paths `Path::starts_with` takes as equal, as a drive's letter is any case
/// there, so every prefix (its kind and letters) hashes alike and the compare decides.
fn component_hash(hash: u64, component: Component<'_>) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mix = |hash: u64, word: u64| (hash.rotate_left(5) ^ word).wrapping_mul(K);
    let bytes = match component {
        Component::Prefix(_) => return mix(hash, u64::MAX),
        other => other.as_os_str().as_encoded_bytes(),
    };
    let mut words = bytes.chunks_exact(8);
    let mut hash = hash;
    for word in &mut words {
        hash = mix(hash, u64::from_le_bytes(word.try_into().expect("8 bytes")));
    }
    let mut last = [0; 8];
    last[..words.remainder().len()].copy_from_slice(words.remainder());
    mix(mix(hash, u64::from_le_bytes(last)), bytes.len() as u64)
}

fn depth_bit(depth: u32) -> u64 {
    1 << depth.min(63)
}

impl Rule {
    /// The rule pinning `toolchain`, the `build` of blaze that dependency images are keyed by,
    /// and the builders' `systems`, with what is under each of `roots`, and the variables of
    /// `public_env` public. Of two roots of one path the later is taken.
    pub fn new(
        toolchain: ContentId,
        build: ContentId,
        systems: Vec<ContentId>,
        roots: Vec<Root>,
        public_env: Vec<OsString>,
    ) -> Rule {
        let mut by_hash = Vec::with_capacity(roots.len());
        let mut depths = 0;
        for (at, root) in roots.iter().enumerate() {
            let (hash, depth) = root.path.components().fold((0, 0), |(hash, depth), it| (component_hash(hash, it), depth + 1));
            depths |= depth_bit(depth);
            by_hash.push((hash, depth, at as u32));
        }
        by_hash.sort_unstable();
        Rule { toolchain, build, systems, roots, by_hash, depths, public_env }
    }

    pub fn toolchain(&self) -> ContentId {
        self.toolchain
    }

    pub fn build(&self) -> ContentId {
        self.build
    }

    pub fn systems(&self) -> &[ContentId] {
        &self.systems
    }

    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    pub fn public_env(&self) -> &[OsString] {
        &self.public_env
    }

    /// Whether `origin` is public here.
    pub fn origin(&self, origin: &Origin) -> Result<(), String> {
        match origin {
            Origin::CratesIo { name, version, .. } if !name.is_empty() && !version.is_empty() => Ok(()),
            Origin::CratesIo { .. } => Err("a crates.io crate with no name or version".into()),
            Origin::Toolchain(id) if *id == self.toolchain => Ok(()),
            // alloc: an error's message, made only when it fails.
            Origin::Toolchain(id) => Err(format!(
                "it reads the toolchain {:x}, not the pinned {:x}",
                id, self.toolchain
            )),
            Origin::System(id) if self.systems.contains(id) => Ok(()),
            // alloc: an error's message, made only when it fails.
            Origin::System(id) => Err(format!("it reads the system image {:x}, which is not a builder's", id)),
            Origin::Outputs => Ok(()),
            // alloc: an error's message, made only when it fails.
            private => Err(format!("it reads {private}")),
        }
    }

    /// Where `path` comes from: the origin of the longest root it is under. One walk of its
    /// components, a probe at each depth some root has, and one compare of each root found.
    pub fn path(&self, path: &Path) -> Result<&Origin, String> {
        let probe = |hash: u64, depth: u32| {
            if self.depths & depth_bit(depth) == 0 {
                return None;
            }
            let from = self.by_hash.partition_point(|it| (it.0, it.1) < (hash, depth));
            self.by_hash[from..]
                .iter()
                .take_while(|it| (it.0, it.1) == (hash, depth))
                .filter(|it| path.starts_with(&self.roots[it.2 as usize].path))
                .map(|it| it.2)
                .max()
        };
        let mut longest = probe(0, 0);
        let (mut hash, mut depth) = (0, 0);
        for component in path.components() {
            hash = component_hash(hash, component);
            depth += 1;
            longest = probe(hash, depth).or(longest);
        }
        match longest {
            Some(at) => Ok(&self.roots[at as usize].origin),
            // alloc: an error path.
            None => Err(format!("it reads {}, which is under no public source", path.display())),
        }
    }

    /// [`Rule::path`] as the linear scan it is the same as: the tests' reference.
    #[cfg(test)]
    fn path_scanned(&self, path: &Path) -> Option<&Origin> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(&root.path))
            .max_by_key(|root| root.path.components().count())
            .map(|root| &root.origin)
    }

    fn public_path(&self, path: &Path) -> Result<(), String> {
        let origin = self.path(path)?;
        // alloc: an error's message, made only when it fails.
        self.origin(origin).map_err(|why| format!("{why} ({})", path.display()))
    }

    pub fn query(&self, made: &QueryMade<'_>) -> Result<Public, Private> {
        let key = Key::of(made.schema, made.producer, made.inputs);
        let private = |why: String| Private { key, why };
        if made.origins.len() != made.inputs.len() {
            // alloc: an error path.
            return Err(private(format!("{} origins for {} inputs", made.origins.len(), made.inputs.len())));
        }
        for origin in made.origins {
            self.origin(origin).map_err(private)?;
        }
        Ok(Public { key, kind: Kind::Query })
    }

    pub fn run(&self, made: &RunMade<'_>, part: RunPart) -> Result<Public, Private> {
        let key = match part {
            RunPart::Result => index::result_key(&made.run, made.traced),
            RunPart::Shape(n) => index::slot(&made.run, n),
        };
        let private = |why: String| Private { key, why };
        if made.given.finish() != made.run.given {
            return Err(private("what it was given is not what its key was made with".into()));
        }
        // alloc: an error's message, made only when it fails.
        self.origin(made.code).map_err(|why| private(format!("its code: {why}")))?;
        // alloc: an error's message, made only when it fails.
        self.origin(made.caller).map_err(|why| private(format!("what it runs for: {why}")))?;
        for (name, value) in made.given.vars() {
            let path = Path::new(value);
            if path.is_absolute() {
                self.public_path(path)
                    // alloc: an error's message, made only when it fails.
                    .map_err(|why| private(format!("{} names a path: {why}", name.to_string_lossy())))?;
            }
        }
        let traced = made.traced;
        if let Some(it) = traced.untraceable.first() {
            // alloc: an error's message, made only when it fails.
            return Err(private(format!("it read what can't be traced ({})", trace::untraceable_name(*it))));
        }
        for (path, _) in &traced.files {
            self.public_path(path).map_err(private)?;
        }
        for (path, _) in &traced.dirs {
            self.public_path(path).map_err(private)?;
        }
        for (name, value) in &traced.env {
            if name == trace::ALL_ENV_NAME {
                return Err(private("it read the whole environment".into()));
            }
            if value.is_some() && !self.public_env.contains(name) {
                // alloc: an error's message, made only when it fails.
                return Err(private(format!(
                    "it read {} with a value, which is the machine's",
                    name.to_string_lossy()
                )));
            }
        }
        Ok(Public { key, kind: Kind::Run })
    }

    pub fn function(&self, made: &FunctionMade<'_>) -> Result<Public, Private> {
        let instance = made.function.instance.key();
        let stored = with_keys(made.built_in, |built_in| blaze_codegen::driver::stored(instance, built_in));
        let key = keys::release(&made.imports.blob_key(stored));
        let private = |why: String| Private { key, why };
        let spec = made.spec;
        if spec.settings() != made.function.instance.settings {
            return Err(private("its settings are not those of the spec given".into()));
        }
        if !matches!(spec.backend, Backend::Llvm { .. }) {
            return Err(private("it is not compiled by LLVM: dev code is shown public by `Rule::dev_function`".into()));
        }
        if made.imported.len() != made.imports.imported.len()
            || made.imported.iter().zip(&made.imports.imported).any(|(it, key)| it.instance.key() != *key)
        {
            return Err(private("the functions given are not those it imports".into()));
        }
        let all = std::iter::once(made.function).chain(made.built_in).chain(made.imported);
        self.code(spec, all).map_err(private)?;
        Ok(Public { key, kind: Kind::Code })
    }

    /// Whether a dev function is public: as [`Rule::function`], under the key the dev driver
    /// keeps it by, for code compiled for the target's baseline. Code tuned to the machine
    /// that built it (a `-C target-cpu` or `-C target-feature`, a build run where it is built
    /// with the host's features) is not: its key is of those features, which no build of
    /// another machine asks for.
    pub fn dev_function(&self, made: &DevFunctionMade<'_>) -> Result<Public, Private> {
        let instance = made.function.instance.key();
        let stored = with_keys(made.built_in, |built_in| blaze_codegen::driver::stored(instance, built_in));
        let key = keys::template(&stored);
        let private = |why: String| Private { key, why };
        let spec = made.spec;
        if spec.settings() != made.function.instance.settings {
            return Err(private("its settings are not those of the spec given".into()));
        }
        if matches!(spec.backend, Backend::Llvm { .. }) {
            return Err(private("it is compiled by LLVM: release code is shown public by `Rule::function`".into()));
        }
        if !spec.toolchain.features.is_empty() {
            // alloc: an error's message, made only when it fails.
            return Err(private(format!(
                "it is compiled for {} {}, not the target's baseline",
                spec.toolchain.cpu,
                spec.toolchain.features.join(",")
            )));
        }
        self.code(spec, std::iter::once(made.function).chain(made.built_in)).map_err(private)?;
        Ok(Public { key, kind: Kind::Code })
    }

    /// What every function's code needs, whatever compiles it: the pinned toolchain, and the
    /// code and type arguments of each of `sources` from public crates.
    fn code<'s>(&self, spec: &Spec, sources: impl Iterator<Item = &'s FnSource>) -> Result<(), String> {
        if spec.toolchain.id != self.toolchain {
            // alloc: an error's message, made only when it fails.
            return Err(format!(
                "it is compiled by the toolchain {:x}, not the pinned {:x}",
                spec.toolchain.id, self.toolchain
            ));
        }
        for source in sources {
            let what = || source.instance.key().0;
            // alloc: an error's message, made only when it fails.
            self.origin(&source.code).map_err(|why| format!("the code of {:x}: {why}", what()))?;
            for origin in &source.args {
                // alloc: an error's message, made only when it fails.
                self.origin(origin).map_err(|why| format!("a type argument of {:x}: {why}", what()))?;
            }
        }
        Ok(())
    }

    /// Whether a dependency's front-end result made from `base` is public: its key made again
    /// with its owner's function from `base` and `made`'s further inputs. `base` is a public
    /// front-end value's key (an image's or a check's), so the crate and its closure are
    /// covered by it; each further key it names is a [`Public`] too, and the rest (lint flags,
    /// the borrow checker's mode, a constant's stable key) are settings and names in the key,
    /// not inputs of the machine.
    pub fn derived(&self, base: &Public, made: &DerivedMade<'_>) -> Result<Public, Private> {
        let from = base.key.0;
        let key = Key(match made {
            DerivedMade::Bodies { deps, borrowck } => {
                with_ids(deps, |deps| cache::bodies_key(from, deps, *borrowck))
            }
            DerivedMade::Outcome(inputs) => cache::key(from, inputs),
            DerivedMade::Const { item } => hir_ty::consteval::deps::key(from, *item),
        });
        if base.kind != Kind::Query {
            // alloc: an error's message, made only when it fails.
            return Err(Private { key, why: format!("it is made from {:x}, which is not a front-end value", base.key.0) });
        }
        let deps: &[Public] = match made {
            DerivedMade::Bodies { deps, .. } => *deps,
            DerivedMade::Outcome(_) | DerivedMade::Const { .. } => &[],
        };
        if let Some(dep) = deps.iter().find(|it| it.kind != Kind::Query) {
            // alloc: an error's message, made only when it fails.
            return Err(Private { key, why: format!("a dependency's check key {:x} is not a front-end value", dep.key.0) });
        }
        Ok(Public { key, kind: Kind::Query })
    }

    /// The side table (debug info, panic locations) of a public function, by its bytes' ID.
    pub fn side(&self, id: &ContentId, of: &Public) -> Result<Public, Private> {
        let key = keys::side(id);
        if of.kind != Kind::Code {
            // alloc: an error's message, made only when it fails.
            return Err(Private { key, why: format!("a side table of {:x}, which is not code", of.key.0) });
        }
        Ok(Public { key, kind: Kind::Debug })
    }

    /// Whether `part` of a dependency crate's image is public: the key is the pinned build's,
    /// the crate and each crate of its closure is a crates.io version or the toolchain, each
    /// `OUT_DIR` is under the outputs of public runs, and each name the crate read is a file
    /// or listing of a public source or a public variable.
    pub fn image(&self, made: &ImageMade<'_>, part: ImagePart) -> Result<Public, Private> {
        let key = Key(match part {
            ImagePart::Image => made.key,
            ImagePart::Slot(key) | ImagePart::Filed(key) => key,
        });
        let private = |why: String| Private { key, why };
        if made.build != self.build {
            // alloc: an error path, as is each message below.
            return Err(private(format!(
                "it is keyed by the blaze build {:x}, not the pinned {:x}",
                made.build, self.build
            )));
        }
        if made.crates.is_empty() {
            return Err(private("it covers no crate".into()));
        }
        for (at, origin) in made.crates.iter().enumerate() {
            let which = |why: String| match at {
                // alloc: an error path.
                0 => private(format!("the crate: {why}")),
                // alloc: an error path.
                _ => private(format!("a crate it depends on: {why}")),
            };
            match origin {
                Origin::CratesIo { .. } | Origin::Toolchain(_) => self.origin(origin).map_err(which)?,
                // alloc: an error path.
                other => return Err(which(format!("it is {other}, not a crates.io crate or the toolchain"))),
            }
        }
        for &out in made.out_dirs {
            match self.path(out) {
                Ok(Origin::Outputs) => {}
                Ok(other) => {
                    // alloc: an error path.
                    return Err(private(format!(
                        "its OUT_DIR {} is {other}, not the outputs of public build scripts",
                        out.display()
                    )));
                }
                // alloc: an error path.
                Err(why) => return Err(private(format!("its OUT_DIR: {why}"))),
            }
        }
        if part == ImagePart::Image && !made.reads.is_empty() {
            // alloc: an error path.
            return Err(private("a crate that read names besides its key's inputs has no image under its key alone".into()));
        }
        for read in made.reads {
            match *read {
                ImageRead::File(path) | ImageRead::Dir(path) => {
                    // alloc: an error path.
                    self.public_path(path).map_err(|why| private(format!("a name it read: {why}")))?
                }
                ImageRead::Untraceable(it) => {
                    // alloc: an error path.
                    return Err(private(format!("it read what can't be traced ({})", trace::untraceable_name(it))));
                }
                // alloc: an error path.
                ImageRead::Env(name) if name == trace::ALL_ENV_NAME => return Err(private("it read the whole environment".into())),
                ImageRead::Env(name) if self.public_env.iter().any(|it| it.as_os_str() == name) => {}
                ImageRead::Env(name) => {
                    // alloc: an error path.
                    return Err(private(format!(
                        "it read the variable {}, which is the machine's",
                        name.to_string_lossy()
                    )));
                }
            }
        }
        Ok(Public { key, kind: Kind::Query })
    }
}

/// `f` of the instance keys of `sources`, in their order: on the stack for as many bodies as
/// one function builds in.
fn with_keys<R>(sources: &[FnSource], f: impl FnOnce(&[InstanceKey]) -> R) -> R {
    const INLINE: usize = 16;
    if sources.len() > INLINE {
        // alloc: more bodies built into one function than are kept inline.
        let keys: Vec<InstanceKey> = sources.iter().map(|it| it.instance.key()).collect();
        return f(&keys);
    }
    let mut keys = [InstanceKey(ContentId([0; 32])); INLINE];
    for (key, source) in keys.iter_mut().zip(sources) {
        *key = source.instance.key();
    }
    f(&keys[..sources.len()])
}

/// `f` of the content IDs of `keys`, in their order: on the stack for as many dependencies as
/// most crates have.
fn with_ids<R>(keys: &[Public], f: impl FnOnce(&[ContentId]) -> R) -> R {
    const INLINE: usize = 64;
    if keys.len() > INLINE {
        // alloc: a crate with more dependencies than are kept inline.
        let ids: Vec<ContentId> = keys.iter().map(|it| it.key.0).collect();
        return f(&ids);
    }
    let mut ids = [ContentId([0; 32]); INLINE];
    for (id, key) in ids.iter_mut().zip(keys) {
        *id = key.key.0;
    }
    f(&ids[..keys.len()])
}

/// What no published value may hold: the publisher's seed, and every value of the
/// publishing machine's environment but the public ones. Named by where each came from;
/// the secret itself is never in a message.
pub struct Secrets {
    finders: Vec<(String, memmem::Finder<'static>)>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.finders.iter().map(|(name, _)| name)).finish()
    }
}

impl Secrets {
    /// Secrets shorter than this are not looked for: they would be found in most values.
    pub const SHORTEST: usize = 8;

    pub fn new() -> Secrets {
        // alloc: setup: once a set of secrets.
        Secrets { finders: Vec::new() }
    }

    /// `secret`, named `name`. One shorter than [`Secrets::SHORTEST`] is refused.
    pub fn add(&mut self, name: impl Into<String>, secret: &[u8]) -> Result<(), String> {
        let name = name.into();
        if secret.len() < Secrets::SHORTEST {
            // alloc: an error's message, made only when it fails.
            return Err(format!("the secret {name} is shorter than {} bytes", Secrets::SHORTEST));
        }
        self.finders.push((name, memmem::Finder::new(secret).into_owned()));
        Ok(())
    }

    /// The publisher's seed, raw and as hex.
    pub fn publisher(&mut self, publisher: &Publisher) {
        let seed = publisher.seed();
        self.add("the publisher's seed", seed).expect("a seed is 32 bytes");
        self.add("the publisher's seed in hex", crate::sign::hex(seed).as_bytes()).expect("64 bytes");
    }

    /// Every value of `vars` of at least [`Secrets::SHORTEST`] bytes whose name is not in
    /// `public`, named by its variable.
    pub fn env(&mut self, vars: impl IntoIterator<Item = (OsString, OsString)>, public: &[OsString]) {
        for (name, value) in vars {
            if public.contains(&name) {
                continue;
            }
            let bytes = value.as_encoded_bytes();
            if bytes.len() >= Secrets::SHORTEST {
                // alloc: setup: once a set of secrets.
                let name = format!("the value of {}", name.to_string_lossy());
                self.add(name, bytes).expect("long enough");
            }
        }
    }

    /// Refuses `bytes`, the value of `key`, when it holds a secret.
    pub fn check(&self, key: Key, bytes: &[u8]) -> Result<(), Private> {
        match self.finders.iter().find(|(_, finder)| finder.find(bytes).is_some()) {
            // alloc: an error's message, made only when it fails.
            Some((name, _)) => Err(Private { key, why: format!("it holds {name}") }),
            None => Ok(()),
        }
    }
}

impl Default for Secrets {
    fn default() -> Secrets {
        Secrets::new()
    }
}

#[cfg(test)]
mod tests {
    use blaze_codegen::{
        Settings,
        settings::{DebugInfo, Panic, Pgo, Relocation, Sanitize, Toolchain},
        Arch,
    };
    use blaze_core::Untraceable;
    use blaze_items::CodeHash;
    use blaze_usercode::RunKind;

    use super::*;

    const TOOLCHAIN: ContentId = ContentId([1; 32]);
    const SYSTEM: ContentId = ContentId([2; 32]);
    const BUILD: ContentId = ContentId([11; 32]);

    fn serde() -> Origin {
        Origin::CratesIo { name: "serde".into(), version: "1.0.219".into(), checksum: [3; 32] }
    }

    fn rule() -> Rule {
        Rule::new(
            TOOLCHAIN,
            BUILD,
            vec![SYSTEM],
            vec![
                Root { path: "/reg/serde-1.0.219".into(), origin: serde() },
                Root { path: "/toolchain".into(), origin: Origin::Toolchain(TOOLCHAIN) },
                Root { path: "/usr".into(), origin: Origin::System(SYSTEM) },
                Root { path: "/work".into(), origin: Origin::Workspace },
                Root { path: "/work/target/build".into(), origin: Origin::Outputs },
                Root { path: "/reg/serde-1.0.219/vendored".into(), origin: Origin::Path },
            ],
            vec!["TARGET".into()],
        )
    }

    fn private_origins() -> Vec<Origin> {
        vec![
            Origin::Workspace,
            Origin::Path,
            Origin::Git { url: "https://example.com/x.git".into() },
            Origin::Registry { index: "sparse+https://registry.example.com/".into() },
            Origin::Toolchain(ContentId([9; 32])),
            Origin::System(ContentId([9; 32])),
            Origin::CratesIo { name: String::new(), version: "1.0.0".into(), checksum: [0; 32] },
        ]
    }

    #[test]
    fn each_origin_is_public_or_not() {
        let rule = rule();
        for public in [serde(), Origin::Toolchain(TOOLCHAIN), Origin::System(SYSTEM), Origin::Outputs] {
            assert_eq!(rule.origin(&public), Ok(()), "{public}");
        }
        for private in private_origins() {
            assert!(rule.origin(&private).is_err(), "{private}");
        }
    }

    #[test]
    fn a_path_is_what_its_longest_root_says() {
        let rule = rule();
        assert_eq!(rule.path(Path::new("/reg/serde-1.0.219/src/lib.rs")), Ok(&serde()));
        assert_eq!(rule.path(Path::new("/reg/serde-1.0.219/vendored/a.c")), Ok(&Origin::Path));
        assert_eq!(rule.path(Path::new("/work/target/build/x/out/a.rs")), Ok(&Origin::Outputs));
        assert_eq!(rule.path(Path::new("/work/src/main.rs")), Ok(&Origin::Workspace));
        assert!(rule.path(Path::new("/reg/serde-1.0.2190/src/lib.rs")).is_err(), "a root is whole components");
        assert!(rule.path(Path::new("/home/alice/.ssh/id_ed25519")).is_err());
    }

    /// Paths and roots drawn from a few names, so roots nest, repeat and share prefixes.
    fn drawn(rng: &mut blaze_ids::rng::SplitMix64, most: u64) -> PathBuf {
        const NAMES: [&str; 6] = ["reg", "serde", "src", "a.rs", "work", "out"];
        let mut path = PathBuf::from(if rng.next_u64() % 8 == 0 { "rel" } else { "/" });
        for _ in 0..rng.next_u64() % most {
            path.push(NAMES[(rng.next_u64() % NAMES.len() as u64) as usize]);
        }
        path
    }

    #[test]
    fn the_indexed_lookup_is_the_longest_root_scan() {
        let mut rng = blaze_ids::rng::SplitMix64::new(7);
        let origins = [serde(), Origin::Workspace, Origin::Outputs, Origin::Path, Origin::Toolchain(TOOLCHAIN)];
        for round in 0..200 {
            let roots = (0..rng.next_u64() % 12)
                .map(|n| Root { path: drawn(&mut rng, 5), origin: origins[n as usize % origins.len()].clone() })
                .collect();
            let rule = Rule::new(TOOLCHAIN, BUILD, Vec::new(), roots, Vec::new());
            for _ in 0..50 {
                let path = drawn(&mut rng, 8);
                assert_eq!(rule.path(&path).ok(), rule.path_scanned(&path), "round {round}: {}", path.display());
            }
        }
        let doubled = Rule::new(TOOLCHAIN, BUILD, Vec::new(), vec![Root { path: "/a//b/".into(), origin: serde() }], Vec::new());
        assert_eq!(doubled.path(Path::new("/a/b/./c")), Ok(&serde()), "components, not bytes");
        let everything =
            Rule::new(TOOLCHAIN, BUILD, Vec::new(), vec![Root { path: "".into(), origin: Origin::Outputs }], Vec::new());
        assert_eq!(everything.path(Path::new("/x")).ok(), Some(&Origin::Outputs), "an empty root is under everything");
        assert_eq!(everything.path_scanned(Path::new("/x")), Some(&Origin::Outputs));
    }

    #[test]
    fn a_path_is_classified_with_no_allocation() {
        let rule = rule();
        let path = Path::new("/reg/serde-1.0.219/vendored/zlib/src/deflate.c");
        assert_eq!(rule.path(path), Ok(&Origin::Path));
        let (found, used) = blaze_alloc_count::measure(|| rule.path(path).is_ok());
        assert!(found);
        assert_eq!(used, blaze_alloc_count::Used::default(), "a path's origin is found in place");
    }

    #[test]
    fn a_query_is_public_when_every_input_is_and_keyed_as_the_store_keys_it() {
        let rule = rule();
        let schema = SchemaHash(ContentId([4; 32]));
        let producer = ContentId([5; 32]);
        let inputs = [ContentId([6; 32]), ContentId([7; 32])];
        let origins = [serde(), Origin::Toolchain(TOOLCHAIN)];
        let made = QueryMade { schema, producer, inputs: &inputs, origins: &origins };
        let public = rule.query(&made).unwrap();
        assert_eq!(public.key(), Key::of(schema, producer, &inputs));
        assert_eq!(public.kind(), Kind::Query);
        let (again, used) = blaze_alloc_count::measure(|| rule.query(&made).map(|it| it.key()));
        assert_eq!(again, Ok(public.key()));
        assert_eq!(used, blaze_alloc_count::Used::default(), "a public query is shown so with no allocation");
        for private in private_origins() {
            let inputs = [ContentId([6; 32]), ContentId([8; 32])];
            let origins = [serde(), private.clone()];
            let error = rule.query(&QueryMade { schema, producer, inputs: &inputs, origins: &origins }).unwrap_err();
            assert_eq!(error.key, Key::of(schema, producer, &inputs));
        }
        let short = QueryMade { schema, producer, inputs: &inputs, origins: &origins[..1] };
        assert!(rule.query(&short).unwrap_err().why.contains("1 origins for 2 inputs"));
    }

    fn given() -> Given {
        let mut given = Given::new(RunKind::BuildScript);
        given.env("TARGET", "x86_64-unknown-linux-gnu").env("CARGO_MANIFEST_DIR", "/reg/serde-1.0.219");
        given.env("OUT_DIR", "/work/target/build/serde-1/out");
        given
    }

    fn traced() -> Traced {
        Traced {
            files: vec![
                ("/reg/serde-1.0.219/build.rs".into(), Some([1; 32])),
                ("/usr/include/stdio.h".into(), Some([2; 32])),
                ("/toolchain/lib/rustlib/x".into(), None),
            ],
            dirs: vec![("/reg/serde-1.0.219/src".into(), [3; 32])],
            env: vec![("TARGET".into(), Some("x".into())), ("HOME".into(), None)],
            untraceable: Vec::new(),
        }
    }

    fn run_key(given: &Given) -> RunKey {
        RunKey { code: ContentId([9; 32]), given: given.finish() }
    }

    #[test]
    fn a_public_run_is_keyed_as_usercode_keeps_it() {
        let (rule, given, traced) = (rule(), given(), traced());
        let run = run_key(&given);
        let made = RunMade { run, given: &given, code: &serde(), caller: &serde(), traced: &traced };
        let result = rule.run(&made, RunPart::Result).unwrap();
        assert_eq!((result.key(), result.kind()), (index::result_key(&run, &traced), Kind::Run));
        assert_eq!(rule.run(&made, RunPart::Shape(2)).unwrap().key(), index::slot(&run, 2));
    }

    #[test]
    fn a_run_touching_anything_private_is_not_public() {
        let rule = rule();
        let given = given();
        let run = run_key(&given);
        let check = |traced: &Traced, code: &Origin, caller: &Origin, given: &Given| {
            let made = RunMade { run, given, code, caller, traced };
            rule.run(&made, RunPart::Result).err().map(|it| it.why)
        };
        let traced = traced();
        assert_eq!(check(&traced, &serde(), &serde(), &given), None);
        let changes: Vec<(&str, fn(&mut Traced))> = vec![
            ("/work/src/main.rs", |it| it.files.push(("/work/src/main.rs".into(), Some([0; 32])))),
            ("/home", |it| it.files.push(("/home/alice/.cargo/config.toml".into(), None))),
            ("vendored", |it| it.dirs.push(("/reg/serde-1.0.219/vendored".into(), [0; 32]))),
            ("HOME", |it| it.env[1].1 = Some("/home/alice".into())),
            ("whole environment", |it| it.env.push((trace::ALL_ENV_NAME.into(), Some("h".into())))),
            ("traced", |it| it.untraceable.push(Untraceable::Network)),
        ];
        for (says, change) in changes {
            let mut changed = traced.clone();
            change(&mut changed);
            let why = check(&changed, &serde(), &serde(), &given).unwrap_or_else(|| panic!("{says} is public"));
            assert!(why.contains(says), "{why:?} does not say {says:?}");
        }
        for private in private_origins() {
            assert!(check(&traced, &private, &serde(), &given).unwrap().contains("its code"));
            assert!(check(&traced, &serde(), &private, &given).unwrap().contains("runs for"));
        }
        let mut other = self::given();
        other.env("OUT_DIR", "/home/alice/proj/target/out");
        assert!(check(&traced, &serde(), &serde(), &other).unwrap().contains("given"));
        let other_run = RunKey { code: run.code, given: other.finish() };
        let made = RunMade { run: other_run, given: &other, code: &serde(), caller: &serde(), traced: &traced };
        assert!(rule.run(&made, RunPart::Result).unwrap_err().why.contains("OUT_DIR names a path"));
    }

    fn spec(backend: Backend, toolchain: ContentId) -> Spec {
        Spec {
            toolchain: Toolchain {
                id: toolchain,
                target: "x86_64-unknown-linux-gnu".into(),
                arch: Arch::X86_64,
                cpu: "x86-64".into(),
                features: Vec::new(),
                cranelift: "0.136".into(),
                llvm: Some("23.1".into()),
                stencils: None,
                frame_pointers: false,
                relocation: Relocation::Pic,
            },
            backend,
            overflow_checks: false,
            debug_assertions: false,
            panic: Panic::Unwind,
            debuginfo: DebugInfo::None,
            fold: false,
            counted: false,
            mir_inline: false,
            sanitize: Sanitize::default(),
            pgo: Pgo::None,
        }
    }

    fn source(n: u8, settings: Settings, code: Origin) -> FnSource {
        FnSource {
            instance: Instance { code: CodeHash(ContentId([n; 32])), args: ContentId([n + 1; 32]), settings },
            code,
            args: vec![Origin::Toolchain(TOOLCHAIN)],
        }
    }

    #[test]
    fn an_llvm_function_of_public_crates_is_public_and_nothing_else_is() {
        let rule = rule();
        let release = spec(Backend::Llvm { opt: 3 }, TOOLCHAIN);
        let settings = release.settings();
        let function = source(10, settings, serde());
        let built_in = [source(20, settings, Origin::Toolchain(TOOLCHAIN))];
        let imported = [source(30, settings, serde())];
        let imports = Imports { imported: vec![imported[0].instance.key()], ..Imports::default() };
        let made = FunctionMade { spec: &release, function: &function, built_in: &built_in, imports: &imports, imported: &imported };
        let public = rule.function(&made).unwrap();
        let stored = blaze_codegen::driver::stored(function.instance.key(), &[built_in[0].instance.key()]);
        assert_eq!((public.key(), public.kind()), (keys::release(&imports.blob_key(stored)), Kind::Code));
        assert_eq!(rule.side(&ContentId([1; 32]), &public).unwrap().kind(), Kind::Debug);

        let dev = spec(Backend::Cranelift { speed: false }, TOOLCHAIN);
        let dev_fn = source(10, dev.settings(), serde());
        let made_dev = FunctionMade { spec: &dev, function: &dev_fn, ..made };
        assert!(rule.function(&made_dev).unwrap_err().why.contains("LLVM"));

        let other = spec(Backend::Llvm { opt: 3 }, ContentId([9; 32]));
        let other_fn = source(10, other.settings(), serde());
        let made_other = FunctionMade { spec: &other, function: &other_fn, ..made };
        assert!(rule.function(&made_other).unwrap_err().why.contains("pinned"));

        let made_wrong = FunctionMade { spec: &dev, ..made };
        assert!(rule.function(&made_wrong).unwrap_err().why.contains("settings"));

        for private in private_origins() {
            let built_in = [source(20, settings, private.clone())];
            assert!(rule.function(&FunctionMade { built_in: &built_in, ..made }).is_err());
            let mut imported = [source(30, settings, serde())];
            imported[0].args.push(private.clone());
            assert!(rule.function(&FunctionMade { imported: &imported, ..made }).is_err());
        }
        assert!(rule.function(&FunctionMade { imported: &[], ..made }).unwrap_err().why.contains("imports"));
    }

    #[test]
    fn a_dev_function_of_public_crates_for_the_baseline_is_public_under_its_template_key() {
        let rule = rule();
        for backend in [Backend::Cranelift { speed: true }, Backend::Cranelift { speed: false }, Backend::Stencil] {
            let dev = spec(backend, TOOLCHAIN);
            let settings = dev.settings();
            let function = source(10, settings, serde());
            let built_in = [source(20, settings, Origin::Toolchain(TOOLCHAIN))];
            let made = DevFunctionMade { spec: &dev, function: &function, built_in: &built_in };
            let public = rule.dev_function(&made).unwrap();
            let stored = blaze_codegen::driver::stored(function.instance.key(), &[built_in[0].instance.key()]);
            assert_eq!((public.key(), public.kind()), (keys::template(&stored), Kind::Code));
            assert_eq!(rule.side(&ContentId([1; 32]), &public).unwrap().kind(), Kind::Debug);

            let mut native = spec(backend, TOOLCHAIN);
            native.toolchain.features = vec!["+avx2".into()];
            let native_fn = source(10, native.settings(), serde());
            let tuned = DevFunctionMade { spec: &native, function: &native_fn, built_in: &built_in };
            let why = rule.dev_function(&tuned).unwrap_err().why;
            assert!(why.contains("baseline") && why.contains("+avx2"), "{why}");
            assert!(rule.dev_function(&DevFunctionMade { spec: &native, ..made }).unwrap_err().why.contains("settings"));

            let other = spec(backend, ContentId([9; 32]));
            let other_fn = source(10, other.settings(), serde());
            let made_other = DevFunctionMade { spec: &other, function: &other_fn, ..made };
            assert!(rule.dev_function(&made_other).unwrap_err().why.contains("pinned"));

            for private in private_origins() {
                let built_in = [source(20, settings, private.clone())];
                assert!(rule.dev_function(&DevFunctionMade { built_in: &built_in, ..made }).is_err(), "{private}");
                let function = source(10, settings, private.clone());
                let why = rule.dev_function(&DevFunctionMade { function: &function, ..made }).unwrap_err().why;
                assert!(why.contains("the code of"), "{private}: {why}");
            }
        }
        let release = spec(Backend::Llvm { opt: 3 }, TOOLCHAIN);
        let function = source(10, release.settings(), serde());
        let made = DevFunctionMade { spec: &release, function: &function, built_in: &[] };
        assert!(rule.dev_function(&made).unwrap_err().why.contains("Rule::function"));
    }

    #[test]
    fn a_front_end_result_made_from_a_public_value_is_keyed_as_its_owner_keys_it() {
        let rule = rule();
        let crates = [serde()];
        let out_dirs = [Path::new(OUT)];
        let base = rule.image(&image(&crates, &out_dirs, &[]), ImagePart::Image).unwrap();
        let deps = [Public::listed(Key(ContentId([21; 32])), Kind::Query)];

        let bodies = DerivedMade::Bodies { deps: &deps, borrowck: Mode::Nll };
        let check = rule.derived(&base, &bodies).unwrap();
        assert_eq!((check.key(), check.kind()), (Key(cache::bodies_key(IMAGE, &[ContentId([21; 32])], Mode::Nll)), Kind::Query));
        let (again, used) = blaze_alloc_count::measure(|| rule.derived(&base, &bodies).map(|it| it.key()));
        assert_eq!(again, Ok(check.key()));
        assert_eq!(used, blaze_alloc_count::Used::default(), "a check key is shown public with no allocation");
        let polonius = rule.derived(&base, &DerivedMade::Bodies { deps: &deps, borrowck: Mode::Polonius }).unwrap();
        assert_ne!(polonius.key(), check.key());

        let flags = vec!["-Wunused".to_owned(), "--cap-lints=allow".to_owned()];
        let inputs = Inputs { lint_flags: &flags, keep_duplicates: false, borrowck: Mode::Nll };
        let outcome = rule.derived(&check, &DerivedMade::Outcome(inputs)).unwrap();
        assert_eq!((outcome.key(), outcome.kind()), (Key(cache::key(check.key().0, &inputs)), Kind::Query));

        let item = ContentId([22; 32]);
        let value = rule.derived(&check, &DerivedMade::Const { item }).unwrap();
        assert_eq!(value.key(), Key(hir_ty::consteval::deps::key(check.key().0, item)));

        let many: Vec<Public> = (0..100u8).map(|n| Public::listed(Key(ContentId([n; 32])), Kind::Query)).collect();
        let ids: Vec<ContentId> = many.iter().map(|it| it.key().0).collect();
        let wide = rule.derived(&base, &DerivedMade::Bodies { deps: &many, borrowck: Mode::Nll }).unwrap();
        assert_eq!(wide.key(), Key(cache::bodies_key(IMAGE, &ids, Mode::Nll)), "more dependencies than kept inline");

        let code = Public::listed(Key(ContentId([23; 32])), Kind::Code);
        let error = rule.derived(&code, &DerivedMade::Const { item }).unwrap_err();
        assert!(error.why.contains("not a front-end value"), "{error}");
        assert_eq!(error.key, Key(hir_ty::consteval::deps::key(code.key().0, item)));
        let run = [Public::listed(Key(ContentId([24; 32])), Kind::Run)];
        let error = rule.derived(&base, &DerivedMade::Bodies { deps: &run, borrowck: Mode::Nll }).unwrap_err();
        assert!(error.why.contains("a dependency's check key"), "{error}");
    }

    #[test]
    fn secrets_are_found_and_never_named_by_value() {
        let publisher = crate::sign::tests::publisher(42);
        let mut secrets = Secrets::new();
        secrets.publisher(&publisher);
        secrets.env(
            [
                ("GITHUB_TOKEN".into(), "ghp_abcdefghijklmnop".into()),
                ("TARGET".into(), "x86_64-unknown-linux-gnu".into()),
                ("SHORT".into(), "abc".into()),
            ],
            &["TARGET".into()],
        );
        let key = Key(ContentId([1; 32]));
        assert_eq!(secrets.check(key, b"fn main() { println!(\"x86_64-unknown-linux-gnu abc\") }"), Ok(()));
        let error = secrets.check(key, b"token=ghp_abcdefghijklmnop;").unwrap_err();
        assert!(error.why.contains("GITHUB_TOKEN") && !error.why.contains("ghp_"), "{error}");
        let mut seeded = b"head ".to_vec();
        seeded.extend_from_slice(&[42; 32]);
        assert!(secrets.check(key, &seeded).unwrap_err().why.contains("seed"));
        let hex = crate::sign::hex(&[42; 32]);
        assert!(secrets.check(key, hex.as_bytes()).unwrap_err().why.contains("hex"));
        assert!(Secrets::new().add("x", b"short").is_err());
        assert!(!format!("{secrets:?}").contains("ghp_"));
    }

    const IMAGE: ContentId = ContentId([12; 32]);
    const SLOT: ContentId = ContentId([13; 32]);
    const FILED: ContentId = ContentId([14; 32]);
    const OUT: &str = "/work/target/build/serde-1/out";

    fn image<'a>(crates: &'a [Origin], out_dirs: &'a [&'a Path], reads: &'a [ImageRead<'a>]) -> ImageMade<'a> {
        ImageMade { key: IMAGE, build: BUILD, crates, out_dirs, reads }
    }

    #[test]
    fn an_image_of_public_crates_is_public_under_each_of_its_keys() {
        let rule = rule();
        let crates = [serde(), Origin::Toolchain(TOOLCHAIN)];
        let out_dirs = [Path::new(OUT)];
        let made = image(&crates, &out_dirs, &[]);
        let public = rule.image(&made, ImagePart::Image).unwrap();
        assert_eq!((public.key(), public.kind()), (Key(IMAGE), Kind::Query));
        let (again, used) = blaze_alloc_count::measure(|| rule.image(&made, ImagePart::Image).map(|it| it.key()));
        assert_eq!(again, Ok(Key(IMAGE)));
        assert_eq!(used, blaze_alloc_count::Used::default(), "a public image is shown so with no allocation");

        let reads = [
            ImageRead::File(Path::new("/reg/serde-1.0.219/src/table.in")),
            ImageRead::Dir(Path::new("/toolchain/lib")),
            ImageRead::File(Path::new("/usr/include/zlib.h")),
            ImageRead::File(Path::new("/work/target/build/serde-1/out/generated.rs")),
            ImageRead::Env(OsStr::new("TARGET")),
        ];
        let read = image(&crates, &out_dirs, &reads);
        assert_eq!(rule.image(&read, ImagePart::Slot(SLOT)).map(|it| (it.key(), it.kind())), Ok((Key(SLOT), Kind::Query)));
        assert_eq!(rule.image(&read, ImagePart::Filed(FILED)).map(|it| it.key()), Ok(Key(FILED)));
        let alone = rule.image(&read, ImagePart::Image).unwrap_err();
        assert!(alone.why.contains("its key alone"), "{alone}");
        assert_eq!(alone.key, Key(IMAGE));
    }

    #[test]
    fn a_crate_of_each_origin_is_covered_as_its_origin_says() {
        let rule = rule();
        let out_dirs = [Path::new(OUT)];
        for public in [serde(), Origin::Toolchain(TOOLCHAIN)] {
            let crates = [public.clone(), serde()];
            assert!(rule.image(&image(&crates, &out_dirs, &[]), ImagePart::Image).is_ok(), "{public}");
            let closure = [serde(), public.clone()];
            assert!(rule.image(&image(&closure, &out_dirs, &[]), ImagePart::Image).is_ok(), "{public}");
        }
        let not_crates = [Origin::System(SYSTEM), Origin::Outputs];
        for private in private_origins().into_iter().chain(not_crates) {
            let own = [private.clone(), serde()];
            let error = rule.image(&image(&own, &out_dirs, &[]), ImagePart::Image).unwrap_err();
            assert!(error.why.starts_with("the crate: "), "{private}: {error}");
            let closure = [serde(), Origin::Toolchain(TOOLCHAIN), private.clone()];
            for part in [ImagePart::Image, ImagePart::Slot(SLOT), ImagePart::Filed(FILED)] {
                let error = rule.image(&image(&closure, &out_dirs, &[]), part).unwrap_err();
                assert!(error.why.starts_with("a crate it depends on: "), "{private}: {error}");
            }
        }
        assert!(rule.image(&image(&[], &out_dirs, &[]), ImagePart::Image).unwrap_err().why.contains("no crate"));
    }

    #[test]
    fn an_image_of_another_build_or_out_dir_is_not_public() {
        let rule = rule();
        let crates = [serde()];
        let out_dirs = [Path::new(OUT)];
        let other = ImageMade { build: ContentId([9; 32]), ..image(&crates, &out_dirs, &[]) };
        assert!(rule.image(&other, ImagePart::Image).unwrap_err().why.contains("pinned"));
        let outs = [
            ("/work/src/out", "a workspace member"),
            ("/home/alice/out", "no public source"),
            ("/reg/serde-1.0.219/out", "serde 1.0.219"),
            ("target/build/serde-1/out", "no public source"),
        ];
        for (out, says) in outs {
            let out_dirs = [Path::new(out)];
            let error = rule.image(&image(&crates, &out_dirs, &[]), ImagePart::Image).unwrap_err();
            assert!(error.why.contains("OUT_DIR") && error.why.contains(says), "{out}: {error}");
        }
    }

    #[test]
    fn a_name_read_that_is_not_public_makes_the_derived_keys_private() {
        let rule = rule();
        let crates = [serde()];
        let out_dirs = [Path::new(OUT)];
        let reads = [
            (ImageRead::File(Path::new("/work/src/lib.rs")), "a workspace member"),
            (ImageRead::Dir(Path::new("/reg/serde-1.0.219/vendored")), "a path dependency"),
            (ImageRead::File(Path::new("/home/alice/.ssh/id_ed25519")), "no public source"),
            (ImageRead::File(Path::new("src/table.in")), "no public source"),
            (ImageRead::Env(OsStr::new("HOME")), "HOME"),
            (ImageRead::Env(OsStr::new(trace::ALL_ENV_NAME)), "whole environment"),
            (ImageRead::Untraceable(Untraceable::Network), "traced"),
        ];
        for (read, says) in reads {
            let made = image(&crates, &out_dirs, std::slice::from_ref(&read));
            for (part, key) in [(ImagePart::Slot(SLOT), SLOT), (ImagePart::Filed(FILED), FILED)] {
                let error = rule.image(&made, part).unwrap_err();
                assert!(error.why.contains(says), "{read:?}: {error}");
                assert_eq!(error.key, Key(key));
            }
        }
    }
}
