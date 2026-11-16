//! `blaze export`'s one entry: a program's records in, one native executable's bytes out, and
//! what the command prints about it. Nothing is kept; the same input gives the same bytes on
//! every machine.

use core::fmt;

use blaze_code::record::{Space, Symbol};
use blaze_own::{AVec, Alloc, Vec};

use crate::error::ExportError;
use crate::input::{Entry, Panic, Platform, ProgramIn, Tier};
use crate::layout::{Member, lay_out};
use crate::start::{main_of, start};
use crate::{elf, macho};

/// The C names through which a program starts unwinding (the Itanium C++ ABI's level-1 unwinder,
/// which `panic_unwind` calls): a program whose records name none of them never unwinds.
const UNWINDERS: [&str; 3] = ["_Unwind_RaiseException", "_Unwind_Resume", "_Unwind_ForcedUnwind"];

/// What a panic does in the exported file, said in its report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panics {
    /// The program can unwind by its profile, and nothing in it calls the unwinder: no unwind
    /// tables are needed, and none are written.
    NothingUnwinds,
    /// The profile says `panic = "abort"`, and nothing in it calls the unwinder.
    Abort,
}

/// What one export wrote, for the command's report ([`Exported::report`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exported {
    pub platform: Platform,
    /// The program's functions placed, folded ones included.
    pub functions: u32,
    /// Of them, those whose code an equal record placed earlier holds.
    pub folded: u32,
    pub fast: u32,
    pub optimized: u32,
    /// Start records the export made.
    pub made: u32,
    /// C names the loader binds.
    pub imports: u32,
    pub len: u64,
    /// `LC_UUID` or the GNU build id: a digest of the content and the profile's digest.
    pub identity: [u8; 16],
    pub profile: Option<[u8; 32]>,
    pub panics: Panics,
}

fn hex(f: &mut impl fmt::Write, bytes: &[u8]) -> fmt::Result {
    bytes.iter().try_for_each(|b| write!(f, "{b:02x}"))
}

impl Exported {
    /// The lines `blaze export` prints, each ending in a newline. The tier line never claims a
    /// tier the records were not made at.
    pub fn report(&self, f: &mut impl fmt::Write) -> fmt::Result {
        match (self.fast, self.optimized) {
            (fast, 0) => writeln!(f, "exported at the fast tier: {fast} functions; the optimized tier is not built yet")?,
            (0, optimized) => writeln!(f, "exported at the optimized tier: {optimized} functions")?,
            (fast, optimized) => writeln!(
                f,
                "exported: {optimized} functions at the optimized tier, {fast} at the fast tier (the optimized tier is not built yet for those {fast})"
            )?,
        }
        if self.folded > 0 {
            writeln!(f, "folded: {} functions share an equal function's code", self.folded)?;
        }
        match self.panics {
            Panics::NothingUnwinds => writeln!(f, "panics: nothing in the program calls the unwinder, so the file needs no unwind tables")?,
            Panics::Abort => writeln!(f, "panics: abort (the profile's `panic = \"abort\"`); the file has no unwind tables")?,
        }
        match self.profile {
            None => writeln!(f, "profile: none; functions in call-graph order from the entry")?,
            Some(digest) => {
                write!(f, "profile: ")?;
                hex(f, &digest[..6])?;
                writeln!(f, "; its hot functions first")?;
            }
        }
        let form = match self.platform {
            Platform::MacosArm64 => "Mach-O for arm64 macOS, signed ad hoc",
            Platform::LinuxArm64 => "ELF for arm64 Linux",
        };
        write!(f, "wrote {} bytes ({form}), {} C names bound at start, identity ", self.len, self.imports)?;
        hex(f, &self.identity)?;
        writeln!(f)
    }
}

/// Refuses what an exported file cannot yet do right: landing pads (no LSDA rows are written)
/// and any call of the unwinder (no unwind tables are written). Says what a panic does.
fn panics(program: &ProgramIn<'_>) -> Result<Panics, ExportError> {
    let unwinders = UNWINDERS.map(Symbol::of_name);
    for function in program.functions {
        let r = function.record;
        if r.pads() > 0 {
            return Err(ExportError::LandingPads { symbol: function.symbol });
        }
        for k in 0..r.targets() {
            let t = r.target(k);
            if t.space != Space::Outside {
                continue;
            }
            if let Some(i) = unwinders.iter().position(|&u| u == t.symbol) {
                return Err(ExportError::Unwinds { from: function.symbol, name: UNWINDERS[i], panic: program.panic });
            }
        }
    }
    Ok(match program.panic {
        Panic::Unwind => Panics::NothingUnwinds,
        Panic::Abort => Panics::Abort,
    })
}

/// Exports `program` for `platform`: the file's bytes replace `out`'s contents.
pub fn export<'a>(program: &ProgramIn<'_>, platform: Platform, out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<Exported, ExportError> {
    let panics = panics(program)?;
    let has = |symbol: Symbol| program.functions.iter().any(|f| f.symbol == symbol);
    let main = main_of(program.entry);
    if !matches!(program.entry, Entry::Hosted { .. }) && !has(main) {
        return Err(ExportError::NoEntry { symbol: main });
    }
    if let Entry::LangStart { lang_start, .. } = program.entry
        && !has(lang_start)
    {
        return Err(ExportError::NoEntry { symbol: lang_start });
    }
    let start = start(program.entry, platform, alloc)?;
    let mut members: AVec<'_, Member<'_>> = Vec::with_capacity_in(program.functions.len() + start.records.len(), alloc);
    for f in program.functions {
        members.push(Member { symbol: f.symbol, record: f.record, tier: Some(f.tier) });
    }
    for (symbol, record) in start.views() {
        members.push(Member { symbol, record, tier: None });
    }
    let plan = match platform {
        Platform::MacosArm64 => macho::plan(),
        Platform::LinuxArm64 => elf::plan(),
    };
    let laid = lay_out(&members, program.data, start.entry, program.profile, plan, alloc)?;
    let seed = program.profile.map_or([0u8; 32], |p| p.digest);
    let identity = match platform {
        Platform::MacosArm64 => macho::write(&laid, program.name, &seed, out, alloc)?,
        Platform::LinuxArm64 => elf::write(&laid, &seed, out, alloc)?,
    };
    let (mut functions, mut folded, mut fast, mut optimized, mut made) = (0u32, 0u32, 0u32, 0u32, 0u32);
    for p in laid.placed.iter() {
        match p.tier {
            None => made += 1,
            Some(tier) => {
                functions += 1;
                folded += u32::from(p.folded);
                match tier {
                    Tier::Fast => fast += 1,
                    Tier::Optimized => optimized += 1,
                }
            }
        }
    }
    Ok(Exported {
        platform,
        functions,
        folded,
        fast,
        optimized,
        made,
        imports: laid.imports.len() as u32,
        len: out.len() as u64,
        identity,
        profile: program.profile.map(|p| p.digest),
        panics,
    })
}
