//! `blaze export`'s one entry: a program's records in, one native executable's bytes out, and
//! what the command prints about it. Nothing is kept; the same input gives the same bytes on
//! every machine.

use core::fmt;

use blaze_code::record::Symbol;
use blaze_native::Platform as NativePlatform;
use blaze_native::image::Region as NativeRegion;
use blaze_own::{AVec, Alloc, Vec};

use crate::eh_out::{Outside, eh_room_with, eh_sections_with};
use crate::error::ExportError;
use crate::input::{Entry, Panic, Platform, ProgramIn, Tier};
use crate::layout::{Member, TlsPlan, lay_out_with_tls};
use crate::native_merge::{self, MergeError};
use crate::start::{main_of, start};
use crate::{elf, macho};

/// What a panic does in the exported file, said in its report. Unwind tables are written under
/// both strategies (eh_out.rs): std's backtraces walk them under `panic = "abort"` too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panics {
    /// The program can unwind by its profile; `functions` placed functions have unwind entries.
    Unwind { functions: u32 },
    /// The profile says `panic = "abort"`; `functions` have unwind entries, for backtraces.
    Abort { functions: u32 },
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
            Panics::Unwind { functions } => writeln!(f, "panics: unwind tables for {functions} functions")?,
            Panics::Abort { functions } => {
                writeln!(f, "panics: abort (the profile's `panic = \"abort\"`); unwind tables for {functions} functions, for backtraces")?
            }
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
            Platform::Wasm64 => "WebAssembly memory64 for Wasmtime 64 (blaze_host_v1)",
        };
        write!(f, "wrote {} bytes ({form}), {} C names bound at start, identity ", self.len, self.imports)?;
        hex(f, &self.identity)?;
        writeln!(f)
    }
}

/// What a panic does in the file, with the count of functions given unwind entries.
fn panics(program: &ProgramIn<'_>, functions: u32) -> Panics {
    match program.panic {
        Panic::Unwind => Panics::Unwind { functions },
        Panic::Abort => Panics::Abort { functions },
    }
}

/// Exports `program` for `platform`: the file's bytes replace `out`'s contents.
pub fn export<'a>(program: &ProgramIn<'_>, platform: Platform, out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<Exported, ExportError> {
    if platform == Platform::Wasm64 {
        return Err(ExportError::Wasm64RequiresWasmProgram);
    }
    let has =|symbol: Symbol| program.functions.iter().any(|f| f.symbol == symbol);
    let main = main_of(program.entry);
    if !matches!(program.entry, Entry::Hosted { .. }) && !has(main) {
        return Err(ExportError::NoEntry { symbol: main });
    }
    if let Entry::LangStart { lang_start, .. } = program.entry
        && !has(lang_start)
    {
        return Err(ExportError::NoEntry { symbol: lang_start });
    }
    if let Some(tls) = program.tls
        && !has(tls.ender)
    {
        return Err(ExportError::TlsEnderNotPlaced { ender: tls.ender });
    }
    let start = start(program.entry, program.tls, platform, alloc)?;
    let mut members: AVec<'_, Member<'_>> = Vec::with_capacity_in(program.functions.len() + start.records.len(), alloc);
    for f in program.functions {
        members.push(Member { symbol: f.symbol, record: f.record, tier: Some(f.tier) });
    }
    for (symbol, record) in start.views() {
        members.push(Member { symbol, record, tier: None });
    }
    // The native image (native_merge.rs): its libraries first, as header / `.dynamic` room depends on them.
    let natives = match (program.natives, platform) {
        (None, _) => None,
        (Some(_), Platform::Wasm64) => return Err(ExportError::Wasm64RequiresWasmProgram),
        (Some(image), Platform::MacosArm64) => {
            if image.platform != NativePlatform::MacosArm64 {
                return Err(ExportError::Native(MergeError::Platform));
            }
            Some((image, native_merge::libraries(image, alloc).map_err(ExportError::Native)?))
        }
        (Some(image), Platform::LinuxArm64) => {
            if image.platform != NativePlatform::LinuxArm64 {
                return Err(ExportError::Native(MergeError::Platform));
            }
            Some((image, native_merge::libraries(image, alloc).map_err(ExportError::Native)?))
        }
    };
    let (plan, initial_root) = match platform {
        Platform::MacosArm64 => (macho::plan_for(natives.as_ref().map(|(_, libraries)| &libraries[..])), 0u64),
        Platform::LinuxArm64 => {
            let libs = natives.as_ref().map(|(_, libraries)| &libraries[..]);
            let has_inits = natives.as_ref().is_some_and(|(image, _)| image.region(NativeRegion::Inits).is_some_and(|r| r.len > 0));
            let root_off = elf::tls_root_offset(natives.as_ref().map(|(image, _)| *image)).map_err(ExportError::Native)?;
            (elf::plan_for(libs, has_inits), root_off | blaze_code::tls::ROOT_OWN)
        }
        Platform::Wasm64 => return Err(ExportError::Wasm64RequiresWasmProgram),
    };
    let tls_plan = program.tls.map(|t| TlsPlan { descriptor: t.descriptor, mod_init_fn: start.tls_init, initial_root });
    let room = eh_room_with(&members, program.personality, platform, program.natives, alloc)?;
    let laid = lay_out_with_tls(&members, program.data, start.entry, program.profile, plan, room, tls_plan, alloc)?;
    let seed = program.profile.map_or([0u8; 32], |p| p.digest);
    let (eh, identity, bound) = match platform {
        Platform::MacosArm64 => {
            let merged = match natives {
                None => None,
                Some((image, libraries)) => Some(native_merge::merge(&laid, image, program.exports, libraries, alloc).map_err(ExportError::Native)?),
            };
            let outside = match (program.natives, merged.as_ref()) {
                (Some(image), Some(m)) => Some(Outside { image, shift: m.shift }),
                _ => None,
            };
            let eh = eh_sections_with(&laid, &members, program.personality, platform, outside.as_ref(), alloc)?;
            let identity = macho::write_native(&laid, &eh, merged.as_ref(), program.name, &seed, out, alloc)?;
            let bound = merged.as_ref().map_or(laid.imports.len(), |m| m.bound(laid.imports.len()));
            (eh, identity, bound)
        }
        Platform::LinuxArm64 => {
            let merged = match natives {
                None => None,
                Some((image, libraries)) => Some(native_merge::merge_elf(&laid, image, program.exports, libraries, alloc).map_err(ExportError::Native)?),
            };
            let outside = match (program.natives, merged.as_ref()) {
                (Some(image), Some(m)) => Some(Outside { image, shift: m.shift }),
                _ => None,
            };
            let eh = eh_sections_with(&laid, &members, program.personality, platform, outside.as_ref(), alloc)?;
            let identity = elf::write_native(&laid, &eh, merged.as_ref(), &seed, out, alloc)?;
            let bound = merged.as_ref().map_or(laid.imports.len(), |m| m.bound(laid.imports.len()));
            (eh, identity, bound)
        }
        Platform::Wasm64 => return Err(ExportError::Wasm64RequiresWasmProgram),
    };
    let panics = panics(program, eh.functions);
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
        imports: bound as u32,
        len: out.len() as u64,
        identity,
        profile: program.profile.map(|p| p.digest),
        panics,
    })
}
