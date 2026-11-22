//! [`BaseSet`]: one platform's pinned base tool set, and the only way to one of its programs.
//!
//! A step that may run the set's programs takes a [`StepTools`] ([`BaseSet::step`]) and asks it
//! for each program by name ([`StepTools::program`]); a name the set does not hold is
//! [`NotInSet`], the error a machine without the tool gives (`sh: cmake: command not found`),
//! with the package and the step (rule 19), never a search of the machine's `PATH`.
//! [`StepTools`] is linear (`#[blaze::linear]`): the step ends it with [`StepTools::finish`],
//! whose [`ToolsPart`] goes into the step's key. The part holds the set's digest exactly when a
//! program was taken, and what came from the machine with it ([`FromMachine`]); a step that
//! took none holds nothing, so a set's new digest reruns every step that ran one of its
//! programs and no other (base-tools.md, "Rules").

use core::fmt;

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{
    machine::{FromMachine, MachineOs, SdkId},
    pins::{self, Answers, PinsError, PinsFile, Pinned, Platform, Row, hex_of},
    sandbox::{GUEST_BIN, HostRoot},
};

/// The domain a step's [`ToolsPart`] is hashed under; a change to what it holds is a new one.
const PART_DOMAIN: &str = "blaze-base-tools step tools part 1";

/// The set's identity: its archive's SHA-256, which names it on the shelf and in every key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SetDigest(pub [u8; 32]);

/// One platform's base tool set, built and pinned. Its programs are sorted, each once, so two
/// files that list one set in different orders give equal sets.
#[derive(PartialEq, Eq, Debug)]
pub struct BaseSet<'t, 'a> {
    platform: Platform,
    digest: SetDigest,
    size: u64,
    archive: &'t str,
    programs: AVec<'a, &'t str>,
    answers: Answers<'t>,
}

/// Why there is no set to run with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BaseSetError<'t> {
    /// The pin file is not of its schema.
    Pins(PinsError<'t>),
    /// The file has no row for the platform.
    NoRow { platform: Platform },
    /// The platform's digest is still `PENDING`: CI has not built its archive.
    NotBuilt { platform: Platform },
    /// A row given by hand that lists a program twice.
    TwoPrograms { platform: Platform, program: &'t str },
}

impl fmt::Display for BaseSetError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = pins::PATH;
        match self {
            BaseSetError::Pins(error) => fmt::Display::fmt(error, f),
            BaseSetError::NoRow { platform } => write!(f, "{path} has no base tool set for {platform} (D192)"),
            BaseSetError::NotBuilt { platform } => write!(f, "the base tool set for {platform} is not built yet: {path}"),
            BaseSetError::TwoPrograms { platform, program } => {
                write!(f, "the base tool set for {platform} lists `{program}` twice: {path} (D192)")
            }
        }
    }
}

impl core::error::Error for BaseSetError<'_> {}

impl<'t> From<PinsError<'t>> for BaseSetError<'t> {
    fn from(error: PinsError<'t>) -> BaseSetError<'t> {
        BaseSetError::Pins(error)
    }
}

/// The package and the step a program is asked for in: what [`NotInSet`] names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StepAt<'n> {
    /// The package whose build the step is part of (`tikv-jemalloc-sys`).
    pub package: &'n str,
    /// The step, as its runner names it (`build script`, `sh configure`, `make`).
    pub step: &'n str,
}

/// A program the set does not hold: what a machine without it says, then which package and step
/// asked and that the pinned set is why (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotInSet<'n> {
    pub name: &'n str,
    pub at: StepAt<'n>,
    pub platform: Platform,
}

impl fmt::Display for NotInSet<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let NotInSet { name, at, platform } = self;
        write!(
            f,
            "sh: {name}: command not found (package `{}`, step `{}`: the base tool set for {platform} has no `{name}`, {}; D192)",
            at.package,
            at.step,
            pins::PATH
        )
    }
}

impl core::error::Error for NotInSet<'_> {}

/// A set asked to run on a machine of another kind, or a step's SDK refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WrongMachine<'m> {
    /// An Apple set on a Linux machine, or the other way round.
    Kind { platform: Platform },
    /// An SDK read on a platform that has none.
    SdkNotApple { platform: Platform },
    /// A second, different SDK read by one step.
    TwoSdks { first: SdkId<'m>, second: SdkId<'m> },
}

impl fmt::Display for WrongMachine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WrongMachine::Kind { platform } => write!(f, "the base tool set for {platform} is not this machine's kind (D192)"),
            WrongMachine::SdkNotApple { platform } => write!(f, "a step on {platform} read an Apple SDK, which only an Apple platform has (D192)"),
            WrongMachine::TwoSdks { first, second } => write!(
                f,
                "one step read two Apple SDKs, {} ({}) and {} ({}): a step's key holds one (D192)",
                first.version(),
                first.build(),
                second.version(),
                second.build()
            ),
        }
    }
}

impl core::error::Error for WrongMachine<'_> {}

impl<'t, 'a> BaseSet<'t, 'a> {
    /// The set of `row`, held in `alloc`: refused by name while its digest is `PENDING`.
    pub fn from_row(row: &Row<'t, '_>, alloc: Alloc<'a>) -> Result<BaseSet<'t, 'a>, BaseSetError<'t>> {
        let platform = row.platform;
        let (Pinned::Is(digest), Pinned::Is(size)) = (row.digest, row.size) else {
            return Err(BaseSetError::NotBuilt { platform });
        };
        let mut programs: AVec<'a, &'t str> = Vec::new_in(alloc);
        for group in row.groups.iter() {
            programs.extend(group.programs.iter().copied());
        }
        programs.sort_unstable();
        if let Some(pair) = programs.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(BaseSetError::TwoPrograms { platform, program: pair[0] });
        }
        Ok(BaseSet { platform, digest: SetDigest(digest), size, archive: row.archive, programs, answers: row.answers })
    }

    /// `platform`'s set in `file`.
    pub fn from_file(file: &PinsFile<'t, '_>, platform: Platform, alloc: Alloc<'a>) -> Result<BaseSet<'t, 'a>, BaseSetError<'t>> {
        let row = file.row(platform).ok_or(BaseSetError::NoRow { platform })?;
        BaseSet::from_row(row, alloc)
    }

    /// The program named `name`, if the set holds it. Crate-private: a step reaches a program
    /// only through [`StepTools::program`], which marks the set used.
    fn lookup(&self, name: &str) -> Option<&'t str> {
        let at = self.programs.binary_search_by(|have| str::cmp(have, name)).ok()?;
        Some(self.programs[at])
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    pub fn digest(&self) -> SetDigest {
        self.digest
    }

    /// The archive's size in bytes: the bound its fetch is held to.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The archive's file name.
    pub fn archive(&self) -> &'t str {
        self.archive
    }

    /// Every program, sorted.
    pub fn programs(&self) -> &[&'t str] {
        &self.programs
    }

    pub fn answers(&self) -> Answers<'t> {
        self.answers
    }

    /// What the set adds to a key: its digest, and nothing else of it (not its programs one by
    /// one, its size or its file name; base-tools.md: "the key of a step holds the set's
    /// digest, not each program's"). A step takes it only through [`StepTools::finish`].
    pub fn key_part(&self) -> [u8; 32] {
        self.digest.0
    }

    /// The set's folder's name on the toolchain shelf: `base-tools-<digest>`, so two sets never
    /// share one and the name says which bytes are in it.
    pub fn folder_name<'b>(&self, alloc: Alloc<'b>) -> AString<'b> {
        let mut hex = [0u8; 64];
        let mut out: AString<'b> = String::new_in(alloc);
        out.push_str("base-tools-");
        out.push_str(hex_of(&self.digest.0, &mut hex));
        out
    }

    /// A step's use of the set on `os`: refused when `os` is not the set's platform's kind.
    pub fn step<'s>(&'s self, os: MachineOs<'s>) -> Result<StepTools<'s, 't, 'a>, WrongMachine<'s>> {
        let platform = self.platform;
        match (platform.is_apple(), os) {
            (true, MachineOs::Apple(_)) | (false, MachineOs::Linux) => Ok(StepTools { set: self, os, ran: false, sdk: None }),
            _ => Err(WrongMachine::Kind { platform }),
        }
    }
}

impl<'a> BaseSet<'static, 'a> {
    /// `platform`'s set in the checked-in `pins/base-tools.json` ([`blaze_pins::BASE_TOOLS_JSON`]).
    pub fn checked_in(platform: Platform, alloc: Alloc<'a>) -> Result<BaseSet<'static, 'a>, BaseSetError<'static>> {
        let file = PinsFile::parse(blaze_pins::BASE_TOOLS_JSON, alloc)?;
        BaseSet::from_file(&file, platform, alloc)
    }
}

/// One program of the set, as a step may start it. Made only by [`StepTools::program`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Program<'s> {
    name: &'s str,
}

impl<'s> Program<'s> {
    pub fn name(&self) -> &'s str {
        self.name
    }

    /// Its path as the step's key and records spell it: `/tools/bin/<name>`, the same on every
    /// machine (rule 25).
    pub fn guest_path<'b>(&self, alloc: Alloc<'b>) -> AString<'b> {
        let mut out: AString<'b> = String::new_in(alloc);
        out.push_str(GUEST_BIN);
        out.push('/');
        out.push_str(self.name);
        out
    }

    /// Its path on this machine, which the sandbox starts: `<root>/bin/<name>`.
    pub fn host_path<'b>(&self, root: &HostRoot<'_>, alloc: Alloc<'b>) -> AString<'b> {
        let mut out: AString<'b> = String::new_in(alloc);
        out.push_str(root.path());
        out.push_str("/bin/");
        out.push_str(self.name);
        out
    }
}

/// A step's use of the set: the only giver of a [`Program`], ended by [`StepTools::finish`]
/// (whose part the step's key takes) or [`StepTools::failed`] (a step that kept nothing). It is
/// linear: a step cannot let it go without saying which.
#[blaze::linear]
#[must_use = "a step that took the base tool set ends it with finish(), whose part its key takes, or failed()"]
#[derive(Debug)]
pub struct StepTools<'s, 't, 'a> {
    set: &'s BaseSet<'t, 'a>,
    os: MachineOs<'s>,
    /// Whether a program was taken.
    ran: bool,
    /// The Apple SDK the step read, if any.
    sdk: Option<SdkId<'s>>,
}

impl<'s> StepTools<'s, '_, '_> {
    /// The program `name`, which the step may now start; the set is marked used. A name the set
    /// does not hold is [`NotInSet`] (a path is not a name: the caller starts a program of its
    /// own outputs by its path, never through here).
    pub fn program<'n>(&mut self, name: &'n str, at: StepAt<'n>) -> Result<Program<'s>, NotInSet<'n>> {
        match self.set.lookup(name) {
            Some(name) => {
                self.ran = true;
                Ok(Program { name })
            }
            None => Err(NotInSet { name, at, platform: self.set.platform }),
        }
    }

    /// The step read the Apple SDK `sdk` (a compile's headers, libclang's): held in its part.
    pub fn read_sdk(&mut self, sdk: SdkId<'s>) -> Result<(), WrongMachine<'s>> {
        let platform = self.set.platform;
        if !platform.is_apple() {
            return Err(WrongMachine::SdkNotApple { platform });
        }
        match self.sdk {
            Some(first) if first != sdk => Err(WrongMachine::TwoSdks { first, second: sdk }),
            _ => {
                self.sdk = Some(sdk);
                Ok(())
            }
        }
    }

    /// The step is done and keeps its record: the part its key takes.
    #[blaze::consumes]
    pub fn finish(self) -> ToolsPart {
        let StepTools { set, os, ran, sdk } = self;
        // In tag order (FromMachine::tag): the SDK, then the system libraries.
        let mut machine: [Option<FromMachine<'_>>; 2] = [sdk.map(FromMachine::AppleSdk), None];
        if let (true, MachineOs::Apple(build)) = (ran, os) {
            machine[1] = Some(FromMachine::SystemLibraries(build));
        }
        if !ran && sdk.is_none() {
            return ToolsPart { id: None };
        }
        let mut hasher = blaze_blake3::Hasher::new_derive_key(PART_DOMAIN);
        hasher.update(&[u8::from(ran)]);
        if ran {
            hasher.update(&set.key_part());
        }
        for entry in machine.iter().flatten() {
            entry.identity_into(&mut |bytes: &[u8]| {
                hasher.update(bytes);
            });
        }
        ToolsPart { id: Some(hasher.finalize()) }
    }

    /// The step failed and keeps no record, so no key is made for it.
    #[blaze::consumes]
    pub fn failed(self) {
        let StepTools { .. } = self;
    }
}

/// What a step's key holds of the base tool set: made by [`StepTools::finish`], or
/// [`ToolsPart::NOT_GIVEN`] for a step never given the set (a pure wasm script). Equal for two
/// steps exactly when both ran no program and read no SDK, or both ran with one set (and one OS
/// build on Apple) and read one SDK.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ToolsPart {
    id: Option<[u8; 32]>,
}

impl ToolsPart {
    /// A step that was never given the set: it holds nothing, and its key is what it was before
    /// the set existed.
    pub const NOT_GIVEN: ToolsPart = ToolsPart { id: None };

    /// The 32 bytes the step's key hashes, or `None`: then the key hashes nothing for the set.
    pub const fn id(&self) -> Option<[u8; 32]> {
        self.id
    }
}
