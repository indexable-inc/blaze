//! Answering an edit without linking again: the executable of the last full link stays as it
//! is, and the functions that changed are added to its end.
//!
//! A full link of dx writes half a gigabyte to replace a few kilobytes of code. The splice
//! writes those kilobytes: it lays the patch object out behind everything the linker loaded,
//! resolves what it refers to against the executable's symbols, and writes a jump to the new
//! code over the entry of each function the patch replaces. Whatever referred to the old
//! function, a call, a vtable or a function pointer, still arrives there and goes on to the
//! new one. Data has no entry to jump from, so a static with another value is written where
//! the old one is. The patch names one that no longer fits there as a static new since the
//! base, and holds everything that refers to it.
//!
//! What is appended, in four segments of its own:
//!
//! 1. Read-only: the search table of the unwinder with the patch's frames added, the
//!    patch's constants and unwind tables, and the load-time relocations with the patch's
//!    added.
//! 2. Code: the patch's functions, and a stub for each import they call directly.
//! 3. Data: the patch's writable sections, an address slot for each symbol the code
//!    reaches through one, and one for the loader to fix up in place of each address a
//!    static no longer stores.
//! 4. The program headers with the new segments among them. A kernel before Linux 5.18, and
//!    qemu's user mode, tell the loader they are as far into memory as into the file
//!    whatever the headers say, so they are loaded exactly there: behind everything else,
//!    where nothing has to reach them.
//!
//! Behind those, not loaded, is what a debugger reads of the new code: see [`debug`].
//!
//! The patch holds everything that differs from the object the executable was linked from, so
//! each splice starts from the executable as the linker left it: the last one is undone
//! first. Anything the splice has no answer for is an error, and the caller links in full.
//! No size is such an error: what is sized for what, and the rules for a change to it, are in
//! docs/architecture.md ("No size is a reason to link in full").
//!
//! That is the splice of an ELF executable. A wasm module is written again instead, with
//! the patch's functions in place of its own: see [`wasm`].

pub use blaze_in_place::{copy_file, unwritten};

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the name of a static a patch defines anew starts with: the code generator's
/// `hir_ty_codegen::LASTING_STATIC`, which a test of the engine holds this equal to.
pub const LASTING_STATIC: &str = "blaze::static::lasting::";

/// What the name of a data object of a patch starts with that says the patch left the base's
/// copy of a static behind: the code generator's `hir_ty_codegen::MOVED_STATIC`, which a test
/// of the engine holds this equal to.
pub const MOVED_STATIC: &str = "blaze::static::moved::";

/// Milliseconds since the epoch, as the engine's `live` counts.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| it.as_millis() as u64)
}

pub mod wasm;

use std::{
    fs::{File, OpenOptions},
    mem::offset_of,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    thread::JoinHandle,
    time::SystemTime,
};

use anyhow::{Context, bail, ensure};
use object::{
    Endianness, Object, ObjectSection, ObjectSymbol, ObjectSymbolTable, RelocationFlags,
    RelocationTarget, SectionIndex, SymbolIndex, SymbolKind, elf,
    read::elf::{ElfFile64, FileHeader, ProgramHeader},
};
use rustc_hash::{FxHashMap, FxHashSet};

pub use self::lasting::Lasting;
use self::lasting::{Own, is_lasting, left_behind};

pub mod debug;
pub mod image;
pub mod lasting;
pub mod macho;
pub mod pe;
pub mod sha256;

/// The executable of the last full link, which is read on a thread of its own while the
/// daemon waits for the next edit.
pub struct Linked(JoinHandle<anyhow::Result<Baseline>>);

impl Linked {
    /// Starts reading the executable the linker just wrote at `binary` from `patch` and the
    /// rest of the link line. The patch is read here and now: the next edit writes another.
    pub fn read(binary: PathBuf, patch: &Path, object: &Path) -> Linked {
        let (patch, object) = (std::fs::read(patch), object.to_owned());
        // Taken before the daemon answers: whoever is given the program waits for the write.
        let lock = Writing::ahead(&binary);
        Linked(std::thread::spawn(move || {
            Baseline::parse(&binary, Some(&patch?), &object, false)?.laid_out(binary, lock)
        }))
    }

    /// As [`Linked::read`], of an executable linked with the object alone, no patch: a patch
    /// is then spliced into it with the read baseline. Read here and now, for the splice.
    pub fn read_bare(binary: PathBuf, object: &Path) -> anyhow::Result<Baseline> {
        let lock = Writing::ahead(&binary);
        Baseline::parse(&binary, None, object, false)?.laid_out(binary, lock)
    }

    pub fn baseline(self) -> anyhow::Result<Baseline> {
        self.0.join().map_err(|_| anyhow::anyhow!("reading the executable panicked"))?
    }
}

/// An executable as the linker wrote it, and what the last splice changed of it.
pub struct Baseline {
    path: PathBuf,
    /// Shared with the [`Baseline::view`]s an edit is published with while a splice runs.
    image: Arc<Executable>,
    /// The functions of the patch the executable was linked with. A patch without one of
    /// them means the function is as it first was again, which the executable no longer has.
    baked: Vec<Box<[u8]>>,
    /// What the last splice wrote over, to put back before the next.
    undo: Vec<(u64, Vec<u8>)>,
    /// What the linker wrote where any splice since has written, by address: a program
    /// started from a spliced executable has the splice's jumps in memory, and an edit
    /// that puts a function back as it first was has the linker's bytes put back there.
    originals: FxHashMap<u64, Vec<u8>>,
    /// What the last splice left behind what the linker wrote, which the next one mostly
    /// writes again as it is.
    left: Left,
    /// Where what a debugger reads of an ELF executable is.
    copies: debug::Copies,
    /// What the file was when we last left it: someone else may have written it since.
    stamp: Stamp,
    /// What the last splice of a Mach-O executable appended, with the hashes of its pages.
    appended: Option<macho::Appended>,
    /// The statics new in each of the last splices, with when it was written: a program
    /// started from the executable then has them where the splice put them.
    spliced: Vec<(u64, Lasting)>,
}

/// What a splice appended that the next may find where it is.
#[derive(Default)]
struct Left {
    /// What was appended, and where.
    appended: Option<(u64, Vec<u8>)>,
}

impl Left {
    /// Writes `appended` at `offset`, but for the parts that are there already.
    fn append(&self, file: &File, offset: u64, appended: &[u8]) -> std::io::Result<()> {
        const PART: usize = 1 << 16;
        let old = match &self.appended {
            Some((at, old)) if *at == offset => old.as_slice(),
            _ => &[],
        };
        let mut start = 0;
        while start < appended.len() {
            let same = |at: usize| {
                let end = (at + PART).min(appended.len());
                old.get(at..end) == Some(&appended[at..end])
            };
            if same(start) {
                start += PART;
                continue;
            }
            let mut end = start + PART;
            while end < appended.len() && !same(end) {
                end += PART;
            }
            let end = end.min(appended.len());
            write_all_at(file, &appended[start..end], offset + start as u64)?;
            start = end;
        }
        Ok(())
    }
}

/// An executable by the format it is in, which is what its splice depends on.
enum Executable {
    Elf(Image),
    MachO(macho::Image),
    Pe(pe::Image),
    Wasm(wasm::Module),
}

/// What a splice did, for the log.
pub struct Spliced {
    pub redirected: usize,
    pub appended: usize,
    /// What it copied of the executable to further on: what a debugger reads, which the
    /// first splice after a link finds copied already.
    pub copied: u64,
}

/// An executable as [`Baseline::parse`] read it, before [`Parsed::laid_out`] writes it.
struct Parsed {
    image: Executable,
    baked: Vec<Box<[u8]>>,
    stamp: Stamp,
}

impl Parsed {
    /// The baseline of the executable at `path`, which is written in place under `lock`
    /// ([`Writing::ahead`]) to leave room for what splices add.
    fn laid_out(self, path: PathBuf, lock: Option<File>) -> anyhow::Result<Baseline> {
        let Parsed { image, baked, stamp } = self;
        let (copies, stamp) = match &image {
            Executable::Elf(image) => {
                let copies = laid_out(&path, image, lock).unwrap_or_else(|why| {
                    eprintln!("splice: what a debugger reads stays where the linker put it: {why}");
                    image.described.as_linked()
                });
                (copies, Stamp::of(&File::open(&path)?)?)
            }
            _ => (debug::Copies::default(), stamp),
        };
        Ok(Baseline {
            path,
            image: Arc::new(image),
            baked,
            undo: Vec::new(),
            originals: FxHashMap::default(),
            left: Left::default(),
            copies,
            stamp,
            appended: None,
            spliced: Vec::new(),
        })
    }
}

impl Baseline {
    /// Reads the executable at `path`, which was linked with `patch` ahead of `object`, or
    /// with `object` alone.
    fn parse(
        path: &Path,
        patch: Option<&[u8]>,
        object: &Path,
        rustcs: bool,
    ) -> anyhow::Result<Parsed> {
        let file = File::open(path)?;
        let stamp = Stamp::of(&file)?;
        // SAFETY: the daemon is the one that has the link write the file, and it does not
        // start another while this reads.
        let bytes = unsafe { memmap2::Mmap::map(&file)? };
        if rustcs {
            let image = object::File::parse(&*bytes)?;
            ensure!(
                matches!(image.format(), object::BinaryFormat::Elf | object::BinaryFormat::MachO),
                "only an ELF or Mach-O executable of rustc's is patched as it is"
            );
            // Every object the daemon writes defines symbols named `blaze.<...>` (`_blaze.<...>`
            // in Mach-O), which no Rust symbol (`_ZN`, `_R`) and no C name is.
            let ours = |name: &[u8]| name.strip_prefix(b"_").unwrap_or(name).starts_with(b"blaze.");
            ensure!(
                !image.symbols().any(|it| it.name_bytes().is_ok_and(ours)),
                "the executable where cargo has the program is one the daemon linked"
            );
        }
        let (image, baked) = if bytes.starts_with(b"\0asm") {
            let baked = patch.map(wasm::defined).transpose()?.unwrap_or_default();
            (Executable::Wasm(wasm::Module::read(bytes.to_vec())?), baked)
        } else {
            let image = match object::FileKind::parse(&*bytes)? {
                object::FileKind::MachO64 => Executable::MachO(macho::Image::read(&bytes)?),
                object::FileKind::Pe64 => {
                    let object = std::fs::read(object)?;
                    let objects: Vec<&[u8]> = patch.into_iter().chain([&*object]).collect();
                    Executable::Pe(pe::Image::read(&bytes, path, &objects)?)
                }
                _ => Executable::Elf(Image::read(&bytes)?),
            };
            let baked = match patch {
                Some(patch) => object::File::parse(patch)?
                    .symbols()
                    .filter(|it| it.is_definition() && !it.is_local())
                    .filter_map(|it| it.name_bytes().ok().map(Box::from))
                    .collect(),
                None => Vec::new(),
            };
            (image, baked)
        };
        Ok(Parsed { image, baked, stamp })
    }

    /// What an edit is published with ([`Baseline::live`]) while a splice of the executable
    /// runs on the baseline itself: the program that runs was started before the splice, so
    /// what the splice changes is nothing to it. Not one to splice with.
    pub fn view(&self) -> Baseline {
        Baseline {
            path: self.path.clone(),
            image: Arc::clone(&self.image),
            baked: self.baked.clone(),
            undo: Vec::new(),
            originals: self.originals.clone(),
            left: Left::default(),
            copies: self.copies.clone(),
            stamp: self.stamp.clone(),
            appended: None,
            spliced: self.spliced.clone(),
        }
    }

    pub fn is_of(&self, binary: &Path) -> bool {
        self.path == binary
    }

    /// Whether a splice leaves what a debugger reads of the executable as it was, so that
    /// it knows nothing of the code added. A PDB is a file of its own that only the linker
    /// writes.
    pub fn leaves_debug_info(&self) -> bool {
        matches!(*self.image, Executable::Pe(_))
    }

    /// Brings the executable from what the linker wrote to that with `patch` in place of the
    /// functions it defines. On an error the file may be neither: it has to be linked again.
    /// A Mach-O splice stops when `give_up` is set, with [`GaveUp`], and leaves the executable
    /// and what is known of it as they were.
    pub fn splice(&mut self, patch: &[u8], give_up: &AtomicBool) -> anyhow::Result<Spliced> {
        let before = file_key(&self.path);
        let spliced = self.splice_as(patch, give_up)?;
        names_spliced(&self.path, before);
        Ok(spliced)
    }

    /// [`Baseline::splice`].
    fn splice_as(&mut self, patch: &[u8], give_up: &AtomicBool) -> anyhow::Result<Spliced> {
        self.read_again_if_signed()?;
        let start = std::time::Instant::now();
        let (addition, len, pointed, lasting) = match &*self.image {
            Executable::Elf(image) => {
                let patch = Patch::read(patch, image)?;
                self.has_baked(patch.replaced.iter().map(|it| it.name))?;
                let addition = patch.place(image, &self.copies)?;
                let moves = !addition.described.moved.is_empty();
                let pointed = moves.then(|| image.described.pointed(&addition.described.copies));
                let lasting = Lasting::in_executable(patch.lasting_in(&addition.defined));
                (addition, image.len, pointed, lasting)
            }
            Executable::Pe(image) => {
                let patch = pe::Patch::read(patch, image)?;
                self.has_baked(patch.defined())?;
                (patch.place(image)?, image.len, None, Lasting::default())
            }
            Executable::Wasm(module) => {
                let (bytes, spliced) = self.rewrite(module, patch)?;
                std::fs::write(&self.path, bytes)?;
                self.stamp = Stamp::of(&File::open(&self.path)?)?;
                return Ok(spliced);
            }
            Executable::MachO(image) => {
                let Baseline { path, baked, undo, originals, stamp, appended, .. } = self;
                let (spliced, lasting) = macho::splice(
                    image, path, baked, undo, originals, stamp, appended, patch, give_up,
                )?;
                self.spliced_with(lasting);
                return Ok(spliced);
            }
        };
        let placed = start.elapsed();

        let writing = Writing::open(&self.path)?;
        let file = &writing.file;
        ensure!(
            Stamp::of(&writing.lock)? == self.stamp,
            "the executable was written by someone else"
        );
        for (offset, bytes) in self.undo.drain(..).rev() {
            write_all_at(&file, &bytes, offset)?;
        }
        // Until this is through, nothing is known of what is behind the linker's.
        let left = std::mem::take(&mut self.left);
        let described = &addition.described;
        // First, as the code appended may reach where a section was: each goes where nothing
        // is still to be read, and the headers left when the splice is undone point there.
        let mut copied = 0;
        for run in &described.moved {
            copy_within(&file, run)?;
            copied += run.len;
        }
        if let Some((table, headers)) = &pointed {
            write_all_at(&file, headers, *table)?;
            for run in &described.moved {
                let_go(&file, run.from, run.len);
            }
        }
        self.copies = described.copies.clone();
        let copied_at = start.elapsed();
        left.append(&file, addition.offset, &addition.appended)?;
        write_all_at(&file, &addition.headers, addition.headers_offset)?;
        let appended_at = start.elapsed();
        let mut end = (addition.offset + addition.appended.len() as u64)
            .max(addition.headers_offset + addition.headers.len() as u64);
        for run in &described.copied {
            copy_within(&file, run)?;
            end = end.max(run.to + run.len);
        }
        for (offset, bytes) in &described.written {
            write_all_at(&file, bytes, *offset)?;
            end = end.max(*offset + bytes.len() as u64);
        }
        end = described.moved.iter().map(|it| it.to + it.len).fold(end, u64::max);
        // A file is not cut short behind what a splice wrote: XFS takes a truncation, also to
        // the size the file has, as a reason to write out every page of it not yet written when
        // it is closed, 100 ms of nushell's after a link. Nothing reads what is past `end`.
        if file.metadata()?.len() < end.max(len) {
            file.set_len(end.max(len))?;
        }
        let written_at = start.elapsed();
        for (offset, bytes) in &addition.overwritten {
            let mut old = vec![0; bytes.len()];
            read_exact_at(&file, &mut old, *offset)?;
            self.undo.push((*offset, old));
            write_all_at(&file, bytes, *offset)?;
        }
        self.stamp = Stamp::of(&file)?;
        let redirected_at = start.elapsed();
        writing.done()?;
        if std::env::var_os("BLAZE_STATS").is_some() {
            let us = |it: std::time::Duration| it.as_micros();
            eprintln!(
                "splice-us: place={} copy={} append={} write={} redirect={} close={}",
                us(placed),
                us(copied_at - placed),
                us(appended_at - copied_at),
                us(written_at - appended_at),
                us(redirected_at - written_at),
                us(start.elapsed() - redirected_at)
            );
        }
        let appended = addition.appended.len();
        self.left = Left { appended: Some((addition.offset, addition.appended)) };
        self.spliced_with(lasting);
        Ok(Spliced { redirected: addition.redirected, appended, copied })
    }

    /// Notes that a splice was just written with `lasting` in it.
    fn spliced_with(&mut self, lasting: Lasting) {
        const KEPT: usize = 8;
        let now = now();
        self.spliced.push((now, lasting));
        let over = self.spliced.len().saturating_sub(KEPT);
        self.spliced.drain(..over);
    }

    /// The statics new in the splice that a program started at `started` (milliseconds since
    /// the epoch) runs, which a patch of it keeps: none for one started from what the linker
    /// wrote, or before the splices still known.
    pub fn spliced_lasting(&self, started: u64) -> Lasting {
        let last = self.spliced.iter().rev().find(|it| it.0 <= started);
        last.map(|it| it.1.clone()).unwrap_or_default()
    }

    /// `patch` laid out at `base` of a program that runs the executable as the linker wrote
    /// it, for the program to apply in its memory; the `LC_UUID` of the executable; and where
    /// each function the patch replaces starts in it, with where the executable has it.
    pub fn live(
        &self,
        patch: &[u8],
        base: u64,
        lasting: &mut Lasting,
    ) -> anyhow::Result<([u8; 16], blaze_live::format::Code, Vec<(u64, u64)>)> {
        match &*self.image {
            Executable::MachO(image) => {
                let uuid = macho::uuid(image).context("the executable has no UUID")?;
                let (code, moved) =
                    macho::live(image, &self.baked, &self.originals, patch, base, lasting)?;
                Ok((uuid, code, moved))
            }
            Executable::Elf(image) => {
                ensure!(image.machine == Machine::X86_64, "live edits support Linux x86-64 only");
                let uuid = image.id.context("the executable has no GNU build ID")?;
                let mut patch = Patch::read(patch, image)?;
                self.has_baked(patch.replaced.iter().map(|it| it.name))?;
                lasting.place(&patch.lasting_own()?)?;
                patch.keep_lasting(|name| lasting.address(name))?;
                let (code, moved) =
                    patch.place_live_simple(image, &self.copies, &self.originals, base, lasting)?;
                Ok((uuid, code, moved))
            }
            Executable::Pe(image) => {
                let patch = pe::Patch::read(patch, image)?;
                self.has_baked(patch.defined())?;
                let (code, moved) = patch.place_live(image, base)?;
                Ok((image.guid, code, moved))
            }
            Executable::Wasm(_) => bail!(
                "edits are applied to running programs on macOS AArch64, Linux x86-64 and Windows x86-64 only"
            ),
        }
    }

    /// Whether a patch can call the C symbol `name`, as far as the executable says: an
    /// executable other than Mach-O is not asked.
    pub fn can_call(&self, name: &str) -> bool {
        match &*self.image {
            Executable::MachO(image) => image.can_call(name),
            _ => true,
        }
    }

    /// A symbol the patch `object` refers to that the executable does not give it, as far as
    /// the executable says: an executable other than Mach-O is not asked.
    pub fn missing_in(&self, object: &[u8]) -> Option<String> {
        let Executable::MachO(image) = &*self.image else { return None };
        let file = object::File::parse(object).ok()?;
        file.symbols().filter(|it| it.is_undefined()).find_map(|it| {
            let name = it.name_bytes().ok()?;
            (!image.gives(name)).then(|| String::from_utf8_lossy(name).into_owned())
        })
    }

    /// The `LC_UUID` of the executable, when it is one that has it: the GNU build ID of an
    /// ELF one, the GUID of the PDB of a PE one.
    pub fn uuid(&self) -> Option<[u8; 16]> {
        match &*self.image {
            Executable::MachO(image) => macho::uuid(image),
            Executable::Elf(image) => image.id,
            Executable::Pe(image) => Some(image.guid),
            Executable::Wasm(_) => None,
        }
    }

    /// Takes the signature of a Mach-O executable that was signed again since we last wrote
    /// it, as `codesign` does to give it entitlements, for the splices after.
    /// A view taken before reads the executable as it was signed then.
    pub fn read_again_if_signed(&mut self) -> anyhow::Result<()> {
        let Executable::MachO(_) = &*self.image else { return Ok(()) };
        let file = File::open(&self.path)?;
        let stamp = Stamp::of(&file)?;
        if stamp == self.stamp {
            return Ok(());
        }
        let Some(Executable::MachO(image)) = Arc::get_mut(&mut self.image) else {
            bail!("the executable was signed again while an edit was published with it")
        };
        // SAFETY: as in `read`; whoever signed it is through.
        let bytes = unsafe { memmap2::Mmap::map(&file)? };
        image.signed_again(&bytes, &self.undo)?;
        self.stamp = stamp;
        Ok(())
    }

    /// The splice of a wasm module, which is written anew from the linker's each time.
    fn rewrite(&self, module: &wasm::Module, patch: &[u8]) -> anyhow::Result<(Vec<u8>, Spliced)> {
        self.has_baked(wasm::defined(patch)?.iter().map(|it| &**it))?;
        ensure!(
            Stamp::of(&File::open(&self.path)?)? == self.stamp,
            "the executable was written by someone else"
        );
        module.splice(patch)
    }
}

impl Baseline {
    /// [`has_baked`] of the patch the executable was linked with.
    fn has_baked<'a>(&self, defined: impl IntoIterator<Item = &'a [u8]>) -> anyhow::Result<()> {
        has_baked(&self.baked, defined)
    }
}

/// What [`kept_names`] read, by executable: of the file it was read from (device and inode),
/// and the names.
#[cfg(unix)]
type KeptNames = FxHashMap<PathBuf, ((u64, u64), Arc<FxHashSet<String>>)>;

#[cfg(unix)]
static KEPT_NAMES: std::sync::LazyLock<std::sync::Mutex<KeptNames>> =
    std::sync::LazyLock::new(Default::default);

/// The file at `path`, by its device and inode: a link writes a new one.
#[cfg(unix)]
fn file_key(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let file = std::fs::metadata(path).ok()?;
    Some((file.dev(), file.ino()))
}

#[cfg(not(unix))]
fn file_key(_: &Path) -> Option<(u64, u64)> {
    None
}

/// The names the executable at `path` defines, read once for each file a link writes. A splice
/// moves what some of them are at and adds none, so what was read of the file it started from
/// is true of the one it leaves, which on macOS is a new file ([`names_spliced`]): read again
/// there, a 34 MB program's took 97 ms of each edit (#453).
pub fn kept_names(path: &Path) -> Option<Arc<FxHashSet<String>>> {
    #[cfg(unix)]
    {
        let key = file_key(path)?;
        let mut kept = lock(&KEPT_NAMES);
        if let Some((at, names)) = kept.get(path)
            && *at == key
        {
            return Some(names.clone());
        }
        let bytes = std::fs::read(path).ok()?;
        let file = object::File::parse(&*bytes).ok()?;
        let macho = file.format() == object::BinaryFormat::MachO;
        let names = file.symbols().filter(|it| it.is_definition()).filter_map(|it| {
            let name = it.name().ok()?;
            Some(if macho { name.strip_prefix('_').unwrap_or(name) } else { name }.to_owned())
        });
        let names = Arc::new(names.collect::<FxHashSet<_>>());
        kept.insert(path.to_owned(), (key, names.clone()));
        Some(names)
    }
    #[cfg(not(unix))]
    {
        _ = path;
        None
    }
}

/// Has what [`kept_names`] read of the file `before` at `path` be of the file a splice left
/// there: the same names. What was read of another file, as of a link before, stays of that.
fn names_spliced(path: &Path, before: Option<(u64, u64)>) {
    #[cfg(unix)]
    if let (Some(before), Some(after)) = (before, file_key(path))
        && let Some((at, _)) = lock(&KEPT_NAMES).get_mut(path)
        && *at == before
    {
        *at = after;
    }
    #[cfg(not(unix))]
    {
        _ = (path, before);
    }
}

/// Whether a patch that defines `defined` has each of `baked`, what the patch the executable
/// was linked with defined: one it lacks would be as it first was, which the executable no
/// longer has. A static the patch left behind ([`lasting::left_behind`]) is not one: nothing
/// refers to the executable's copy any more.
pub fn has_baked<'a>(
    baked: &[Box<[u8]>],
    defined: impl IntoIterator<Item = &'a [u8]>,
) -> anyhow::Result<()> {
    let defined: FxHashSet<&[u8]> = defined.into_iter().collect();
    let left: FxHashSet<Vec<u8>> = defined.iter().filter_map(|it| left_behind(it)).collect();
    let lacked = baked.iter().find(|it| {
        !defined.contains(&***it) && !left.contains(&***it) && left_behind(it).is_none()
    });
    if let Some(lacked) = lacked {
        bail!(
            "{}, of the patch the executable was linked with, is as it first was",
            String::from_utf8_lossy(lacked)
        );
    }
    Ok(())
}

/// The `-headerpad` to link the executable at `path` with again for an edit to be spliced into
/// it, when the room its header has is too little: only Apple's linker leaves no more than it
/// is told to. `None` for an executable that has the room, or needs none.
pub fn headerpad_short(path: &Path) -> anyhow::Result<Option<u64>> {
    Ok(headerpad_wanted(path)?.and_then(|(pad, short)| short.then_some(pad)))
}

/// The `-headerpad` of [`headerpad_short`] whether or not the executable at `path` has the
/// room, and whether it is short of it.
pub fn headerpad_wanted(path: &Path) -> anyhow::Result<Option<(u64, bool)>> {
    use std::io::Read as _;
    const HEADER: usize = size_of::<object::macho::MachHeader64<object::LittleEndian>>();
    let mut file = File::open(path)?;
    let mut commands = vec![0; HEADER];
    let magic = object::macho::MH_MAGIC_64.to_le_bytes();
    if file.read_exact(&mut commands).is_err() || commands[..4] != magic {
        return Ok(None);
    }
    let len = u32::from_le_bytes(commands[20..24].try_into()?) as usize;
    commands.resize(HEADER + len, 0);
    file.read_exact(&mut commands[HEADER..])?;
    macho::headerpad_wanted(&commands)
}

/// The `-headerpad` an Apple executable linked of the objects and archives at `paths` wants for
/// splices, at most, from their unwind information alone: what its first link is told, which
/// has no executable of before to go by. `None` when none of them has an AArch64 Mach-O object
/// with unwind information.
pub fn headerpad_of_objects(paths: &[&Path]) -> anyhow::Result<Option<u64>> {
    let mut unwinding = macho::Unwinding::default();
    for path in paths {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: the daemon wrote the objects and the archives are the prebuilt standard
        // library's: nothing writes one while the link that reads it is under way.
        let bytes = unsafe { memmap2::Mmap::map(&file)? };
        let read = |unwinding: &mut macho::Unwinding, object: &[u8]| -> anyhow::Result<()> {
            if object.get(..4) != Some(&object::macho::MH_MAGIC_64.to_le_bytes()) {
                return Ok(());
            }
            let Some([frames, compact]) = macho::unwind_sections(object)? else { return Ok(()) };
            if let Some((offset, size)) = frames {
                let at = usize::try_from(offset)?;
                let section = object
                    .get(at..at + usize::try_from(size)?)
                    .context("`__eh_frame` past the end of the object")?;
                unwinding.add_frames(section)?;
            }
            if let Some((_, size)) = compact {
                unwinding.add_compact(size);
            }
            Ok(())
        };
        let added = match object::read::archive::ArchiveFile::parse(&*bytes) {
            Ok(archive) => archive
                .members()
                .try_for_each(|member| read(&mut unwinding, member?.data(&*bytes)?)),
            Err(_) => read(&mut unwinding, &bytes),
        };
        added.with_context(|| format!("reading the unwind information of {}", path.display()))?;
    }
    Ok(unwinding.headerpad())
}

/// Why a splice stopped half way: a newer edit wants the executable.
#[derive(Debug)]
pub struct GaveUp;

impl std::fmt::Display for GaveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("given up for a newer edit")
    }
}

impl std::error::Error for GaveUp {}

fn go_on(give_up: &AtomicBool) -> Result<(), GaveUp> {
    if give_up.load(std::sync::atomic::Ordering::Relaxed) { Err(GaveUp) } else { Ok(()) }
}

#[cfg(unix)]
fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, offset)
}

#[cfg(unix)]
fn read_exact_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, bytes, offset)
}

#[cfg(windows)]
fn write_all_at(mut file: &File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Seek as _, SeekFrom, Write as _};
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)
}

#[cfg(windows)]
fn read_exact_at(mut file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(bytes)
}

/// Copies what a debugger reads of the executable at `path` to where its splices write it,
/// while the daemon waits for the first edit, and points the section headers there: the
/// splices find it there, and where the linker put it is let go of.
fn laid_out(path: &Path, image: &Image, lock: Option<File>) -> std::io::Result<debug::Copies> {
    let start = std::time::Instant::now();
    let (moved, copies) = image.described.laid_out(image.tail());
    if moved.is_empty() {
        return Ok(copies);
    }
    let writing = match lock {
        Some(lock) => Writing::under(lock, path)?,
        None => Writing::open(path)?,
    };
    let paused = pause_mid_write();
    for run in &moved {
        copy_within(&writing.file, run)?;
    }
    let (table, headers) = image.described.pointed(&copies);
    write_all_at(&writing.file, &headers, table)?;
    let mut linkers: Vec<(u64, u64)> = moved.iter().map(|it| (it.from, it.from + it.len)).collect();
    linkers.sort_unstable();
    let mut runs: Vec<(u64, u64)> = Vec::with_capacity(linkers.len());
    for (start, end) in linkers {
        match runs.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => runs.push((start, end)),
        }
    }
    for (start, end) in runs {
        let_go(&writing.file, start, end - start);
    }
    if paused {
        eprintln!("laid out: written");
    }
    if std::env::var_os("BLAZE_STATS").is_some() {
        let bytes: u64 = moved.iter().map(|it| it.len).sum();
        eprintln!("laid out: {bytes} bytes of debug info in {:.1?}", start.elapsed());
    }
    writing.done()?;
    Ok(copies)
}

/// Lets go of the whole blocks of a run of the file nothing reads any more: the filesystem
/// keeps them no longer, and they read as zeros. Elsewhere than on Linux they stay.
fn let_go(file: &File, from: u64, len: u64) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;
        let (start, end) =
            (from.next_multiple_of(debug::BLOCK), (from + len) / debug::BLOCK * debug::BLOCK);
        if end <= start {
            return;
        }
        let (Ok(start), Ok(len)) = (i64::try_from(start), i64::try_from(end - start)) else {
            return;
        };
        let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        // SAFETY: the descriptor is open for writing, and the range is the file's.
        if unsafe { libc::fallocate(file.as_raw_fd(), mode, start, len) } != 0 {
            eprintln!(
                "splice: what a debugger no longer reads is kept: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        _ = (file, from, len);
    }
}

/// Waits for `BLAZE_LAYOUT_PAUSE_MS` with the executable open for writing, and says so, as
/// `laid_out` says when the write is over: `harness/blaze_cli.py` has a client be given
/// the program meanwhile, every way a user is given one.
fn pause_mid_write() -> bool {
    let pause = std::env::var("BLAZE_LAYOUT_PAUSE_MS").ok().and_then(|it| it.parse().ok());
    if let Some(pause) = pause {
        eprintln!("laid out: paused");
        std::thread::sleep(std::time::Duration::from_millis(pause));
    }
    pause.is_some()
}

/// An executable open to be written in place, on Unix under an exclusive lock on it taken first:
/// on Linux a program started while its file is open for writing is refused ("text file
/// busy"), and whatever is given the program waits for the lock ([`unwritten`]). The file is
/// closed before the lock is let go.
struct Writing {
    file: File,
    lock: File,
    /// The copy that is written in place of the executable a program runs, and where it goes
    /// once written ([`Writing::done`]).
    copy: Option<(PathBuf, PathBuf)>,
}

impl Writing {
    /// The executable at `path`, or a copy of it when a program runs it, which nothing may
    /// write: the program runs on from the file it was started from, and the copy is put at
    /// `path` for the next start.
    fn open(path: &Path) -> std::io::Result<Writing> {
        #[cfg(unix)]
        const BUSY: i32 = libc::ETXTBSY;
        #[cfg(windows)]
        const BUSY: i32 = 32; // ERROR_SHARING_VIOLATION
        let lock = Writing::lock(path)?;
        match OpenOptions::new().read(true).write(true).open(path) {
            Err(busy) if busy.raw_os_error() == Some(BUSY) => {
                let mut copy = path.as_os_str().to_owned();
                copy.push(".splicing");
                let copy = PathBuf::from(copy);
                let file = copy_file(path, &copy)
                    .and_then(|_| OpenOptions::new().read(true).write(true).open(&copy));
                match file {
                    Ok(file) => Ok(Writing { file, lock, copy: Some((copy, path.to_owned())) }),
                    Err(why) => {
                        _ = std::fs::remove_file(&copy);
                        Err(why)
                    }
                }
            }
            opened => Ok(Writing { file: opened?, lock, copy: None }),
        }
    }

    /// Puts the copy written, if it is one, in place of the executable: on Windows, where a
    /// file a program runs cannot be written over, behind the executable moved aside.
    fn done(mut self) -> std::io::Result<()> {
        match self.copy.take() {
            Some((copy, path)) => {
                #[cfg(windows)]
                move_aside(&path)?;
                std::fs::rename(copy, path)
            }
            None => Ok(()),
        }
    }

    /// The lock on the executable a link just wrote at `path`, taken before the daemon answers
    /// for it, of an ELF file: the one written in place after the answer ([`laid_out`]).
    fn ahead(path: &Path) -> Option<File> {
        use std::io::Read as _;
        let mut magic = [0; 4];
        let file = File::open(path).ok()?;
        (&file).read_exact(&mut magic).ok()?;
        if magic != *b"\x7fELF" {
            return None;
        }
        drop(file);
        Writing::lock(path).ok()
    }

    fn lock(path: &Path) -> std::io::Result<File> {
        let lock = File::open(path)?;
        // On Windows a lock keeps out this process's own writes too, and a program that runs
        // is moved aside there rather than refused.
        #[cfg(unix)]
        lock.lock()?;
        Ok(lock)
    }

    fn under(lock: File, path: &Path) -> std::io::Result<Writing> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Writing { file, lock, copy: None })
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        if let Some((copy, _)) = self.copy.take() {
            _ = std::fs::remove_file(copy);
        }
    }
}

/// Moves the executable at `path` aside, which a program runs and Windows lets nothing write
/// or replace: it runs on there. It is deleted once it has exited, by the first move to come
/// after.
#[cfg(windows)]
pub fn move_aside(path: &Path) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let name = path.file_name().unwrap_or_default().to_owned();
    let siblings = path.parent().map(std::fs::read_dir).transpose()?.into_iter().flatten();
    for sibling in siblings {
        let sibling = sibling?;
        let is_aside = sibling.path().extension() == Some("running".as_ref())
            && sibling.file_name().as_encoded_bytes().starts_with(name.as_encoded_bytes());
        if is_aside {
            // The one that still runs stays.
            let _ = std::fs::remove_file(sibling.path());
        }
    }
    static ASIDE: AtomicUsize = AtomicUsize::new(0);
    let aside = ASIDE.fetch_add(1, Ordering::Relaxed);
    let mut moved = path.as_os_str().to_owned();
    moved.push(format!(".{}-{aside}.running", std::process::id()));
    std::fs::rename(path, &moved)
}

/// Writes a run of the file again behind where it is.
fn copy_within(file: &File, moved: &debug::Moved) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if copy_in_kernel(file, moved).is_ok() {
        return Ok(());
    }
    let mut buffer = vec![0; moved.len.min(1 << 20) as usize];
    let mut done = 0;
    while done < moved.len {
        let part = &mut buffer[..(moved.len - done).min(1 << 20) as usize];
        read_exact_at(file, part, moved.from + done)?;
        write_all_at(file, part, moved.to + done)?;
        done += part.len() as u64;
    }
    Ok(())
}

/// Has the kernel copy the run. A filesystem that shares blocks between files (XFS, Btrfs)
/// shares the run's whole blocks rather than copy them: the run is written as far into a
/// block as it was, and the blocks are asked for apart from the part blocks at either end.
#[cfg(target_os = "linux")]
fn copy_in_kernel(file: &File, moved: &debug::Moved) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let (start, end) = (moved.from, moved.from + moved.len);
    let whole = start.next_multiple_of(debug::BLOCK).min(end);
    let wholes = (end / debug::BLOCK * debug::BLOCK).max(whole);
    for (mut from, part_end) in [(start, whole), (whole, wholes), (wholes, end)] {
        while from < part_end {
            let mut source = i64::try_from(from).map_err(std::io::Error::other)?;
            let mut target =
                i64::try_from(moved.to + (from - start)).map_err(std::io::Error::other)?;
            let len = usize::try_from(part_end - from).unwrap_or(usize::MAX).min(1 << 30);
            let fd = file.as_raw_fd();
            // SAFETY: both offsets are the kernel's to read and advance, and the descriptor is open.
            let copied = unsafe { libc::copy_file_range(fd, &mut source, fd, &mut target, len, 0) };
            match copied {
                ..0 => return Err(std::io::Error::last_os_error()),
                0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                copied => from += copied as u64,
            }
        }
    }
    Ok(())
}

#[derive(Clone, PartialEq)]
pub struct Stamp {
    len: u64,
    modified: SystemTime,
}

impl Stamp {
    fn of(file: &File) -> anyhow::Result<Stamp> {
        let meta = file.metadata()?;
        Ok(Stamp { len: meta.len(), modified: meta.modified()? })
    }
}

/// The architecture an executable is of: what its instructions can reach and how the
/// relocations of its objects are numbered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Machine {
    X86_64,
    Aarch64,
}

/// The field a relocation fills in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    /// An address, in 8 bytes.
    Address,
    /// A distance, in 4 bytes.
    Distance,
    /// A distance, in 8 bytes.
    FarDistance,
    /// The distance of a `b` or `bl` in instructions, in 26 bits: 128 MB either way.
    Branch,
    /// The distance of an `adrp` in pages of 4 KB, in 21 bits: 4 GB either way.
    Page,
    /// What of an address `adrp` leaves out, in units of this many bits' worth of bytes:
    /// 0 for an `add`, the size of what is loaded or stored for the others.
    InPage(u8),
}

impl Machine {
    const X16: u32 = 16;

    fn of(e_machine: u16) -> Option<Machine> {
        match e_machine {
            elf::EM_X86_64 => Some(Machine::X86_64),
            elf::EM_AARCH64 => Some(Machine::Aarch64),
            _ => None,
        }
    }

    /// The relocation by which the loader adds where the program is to an address.
    fn relative(self) -> u32 {
        match self {
            Machine::X86_64 => elf::R_X86_64_RELATIVE,
            Machine::Aarch64 => elf::R_AARCH64_RELATIVE,
        }
    }

    /// Whether the loader writes the address of what another library defines for it.
    fn imports(self, r_type: u32) -> bool {
        match self {
            Machine::X86_64 => matches!(r_type, elf::R_X86_64_GLOB_DAT | elf::R_X86_64_JUMP_SLOT),
            Machine::Aarch64 => {
                matches!(r_type, elf::R_AARCH64_GLOB_DAT | elf::R_AARCH64_JUMP_SLOT)
            }
        }
    }

    fn fixup(self, r_type: u32) -> anyhow::Result<(Fixup, Field)> {
        Ok(match self {
            Machine::X86_64 => match r_type {
                elf::R_X86_64_64 => (Fixup::Absolute, Field::Address),
                elf::R_X86_64_PC32 | elf::R_X86_64_PLT32 => (Fixup::Relative, Field::Distance),
                elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX => {
                    (Fixup::ThroughSlot, Field::Distance)
                }
                elf::R_X86_64_TLSGD => (Fixup::ThreadLocal, Field::Distance),
                other => bail!("a relocation of type {other}, which only the linker handles"),
            },
            Machine::Aarch64 => match r_type {
                elf::R_AARCH64_ABS64 => (Fixup::Absolute, Field::Address),
                elf::R_AARCH64_PREL32 => (Fixup::Relative, Field::Distance),
                elf::R_AARCH64_PREL64 => (Fixup::Relative, Field::FarDistance),
                elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 => (Fixup::Relative, Field::Branch),
                elf::R_AARCH64_ADR_PREL_PG_HI21 => (Fixup::Relative, Field::Page),
                elf::R_AARCH64_ADD_ABS_LO12_NC | elf::R_AARCH64_LDST8_ABS_LO12_NC => {
                    (Fixup::Relative, Field::InPage(0))
                }
                elf::R_AARCH64_LDST16_ABS_LO12_NC => (Fixup::Relative, Field::InPage(1)),
                elf::R_AARCH64_LDST32_ABS_LO12_NC => (Fixup::Relative, Field::InPage(2)),
                elf::R_AARCH64_LDST64_ABS_LO12_NC => (Fixup::Relative, Field::InPage(3)),
                elf::R_AARCH64_LDST128_ABS_LO12_NC => (Fixup::Relative, Field::InPage(4)),
                elf::R_AARCH64_ADR_GOT_PAGE => (Fixup::ThroughSlot, Field::Page),
                elf::R_AARCH64_LD64_GOT_LO12_NC => (Fixup::ThroughSlot, Field::InPage(3)),
                other => bail!("a relocation of type {other}, which only the linker handles"),
            },
        })
    }

    /// How long the shortest jump from the entry of a function to its replacement is.
    fn jump(self) -> usize {
        match self {
            // `jmp new`.
            Machine::X86_64 => 5,
            // `b new`.
            Machine::Aarch64 => 4,
        }
    }

    /// The code of a stub, and what it is aligned to. An x86-64 one is `jmp *slot(%rip)`;
    /// an AArch64 one makes an address in `x16` and branches to it.
    fn stub(self) -> (usize, u64) {
        match self {
            Machine::X86_64 => (6, 1),
            Machine::Aarch64 => (12, 4),
        }
    }

    /// Whether a call from `place` reaches `to` without a stub in between.
    fn reaches(self, to: u64, place: u64) -> bool {
        match self {
            Machine::X86_64 => true,
            Machine::Aarch64 => Field::Branch.fill(&mut [0; 4], to, place).is_ok(),
        }
    }

    /// A stub at `stub` that jumps to the address the slot at `slot` holds.
    fn stub_through(self, slot: u64, stub: u64) -> anyhow::Result<Vec<u8>> {
        Ok(match self {
            Machine::X86_64 => {
                let mut code = vec![0xff, 0x25];
                code.extend_from_slice(&relative(slot, stub + 6)?.to_le_bytes());
                code
            }
            // `adrp x16, slot`, `ldr x16, [x16, slot's low bits]`, `br x16`.
            Machine::Aarch64 => {
                let mut code = [0x9000_0000 | Self::X16, 0xf940_0210, 0xd61f_0200]
                    .map(u32::to_le_bytes)
                    .concat();
                Field::Page.fill(&mut code[..4], slot, stub)?;
                Field::InPage(3).fill(&mut code[4..8], slot, stub + 4)?;
                code
            }
        })
    }

    /// Code at `from` that goes on at `to`, where a `b` does not reach: `adrp x16, to`,
    /// `add x16, x16, to's low bits`, `br x16`. A call may change `x16` as it likes.
    fn far_jump(self, to: u64, from: u64) -> anyhow::Result<Vec<u8>> {
        ensure!(self == Machine::Aarch64, "the program is too large");
        let mut code =
            [0x9000_0000 | Self::X16, 0x9100_0210, 0xd61f_0200].map(u32::to_le_bytes).concat();
        Field::Page.fill(&mut code[..4], to, from)?;
        Field::InPage(0).fill(&mut code[4..8], to, from + 4)?;
        Ok(code)
    }

    /// What is written over the entry of a function of `size` bytes at `old` to have it go
    /// on at `new`.
    fn redirect(self, new: u64, old: u64, size: u64) -> anyhow::Result<Vec<u8>> {
        match self {
            Machine::X86_64 => {
                let mut jump = vec![0xe9];
                jump.extend_from_slice(&relative(new, old + 5)?.to_le_bytes());
                Ok(jump)
            }
            Machine::Aarch64 => {
                let mut jump = 0x1400_0000u32.to_le_bytes().to_vec();
                if Field::Branch.fill(&mut jump, new, old).is_ok() {
                    return Ok(jump);
                }
                let jump = self.far_jump(new, old)?;
                ensure!(
                    size >= jump.len() as u64,
                    "a function of {size} bytes, too short to jump {} MB from",
                    new.abs_diff(old) >> 20
                );
                Ok(jump)
            }
        }
    }
}

impl Field {
    /// Writes what the field at `place` has for `to` into its bytes.
    fn fill(self, bytes: &mut [u8], to: u64, place: u64) -> anyhow::Result<()> {
        let instruction = |bytes: &mut [u8], mask: u32, bits: u32| {
            let old = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            bytes[..4].copy_from_slice(&(old & !mask | bits & mask).to_le_bytes());
        };
        let within = |distance: i64, bits: u32| {
            ensure!(
                distance >= -(1 << (bits - 1)) && distance < 1 << (bits - 1),
                "the program is too large"
            );
            Ok(())
        };
        match self {
            Field::Address => bytes[..8].copy_from_slice(&to.to_le_bytes()),
            Field::Distance => bytes[..4].copy_from_slice(&relative(to, place)?.to_le_bytes()),
            Field::FarDistance => bytes[..8].copy_from_slice(&to.wrapping_sub(place).to_le_bytes()),
            Field::Branch => {
                let distance = to.wrapping_sub(place) as i64;
                ensure!(distance % 4 == 0, "a branch to between two instructions");
                within(distance >> 2, 26)?;
                instruction(bytes, 0x03ff_ffff, (distance >> 2) as u32);
            }
            Field::Page => {
                let pages = ((to >> 12) as i64).wrapping_sub((place >> 12) as i64);
                within(pages, 21)?;
                let pages = pages as u32;
                instruction(bytes, 0x60ff_ffe0, (pages & 3) << 29 | (pages >> 2 & 0x7ffff) << 5);
            }
            Field::InPage(scale) => {
                let low = (to & 0xfff) as u32;
                ensure!(
                    low.is_multiple_of(1 << scale),
                    "a load of {} bytes from an address that is not a multiple of that",
                    1 << scale
                );
                instruction(bytes, 0xfff << 10, (low >> scale) << 10);
            }
        }
        Ok(())
    }

    fn len(self) -> usize {
        match self {
            Field::Address | Field::FarDistance => 8,
            _ => 4,
        }
    }
}

/// What a splice has to know of the executable. Nothing of the file is read again.
struct Image {
    len: u64,
    id: Option<[u8; 16]>,
    machine: Machine,
    header: elf::FileHeader64<Endianness>,
    segments: Vec<elf::ProgramHeader64<Endianness>>,
    /// Addresses are relative to where the program is loaded, and the loader adds that to
    /// every address stored in data.
    pie: bool,
    symbols: FxHashMap<Box<[u8]>, Named>,
    /// The address slots the loader fills with what other libraries define.
    imports: FxHashMap<Box<[u8]>, u64>,
    unwind: UnwindTable,
    /// `None` when the loader has no addresses to fix up.
    relocations: Option<LoadRelocations>,
    described: debug::Described,
    /// Where the linker put nothing but traps between two functions, as the address and the
    /// length of each run, in order: what a function too short for a jump hops to
    /// ([`Image::hop`]).
    pads: Vec<(u64, u64)>,
}

/// What a name stands for in the executable.
enum Named {
    One(Defined),
    /// Symbols private to their objects that share the name: which one is meant is lost.
    Many,
}

#[derive(Clone, Copy)]
struct Defined {
    address: u64,
    size: u64,
    /// What a jump written at `address` may cover: `size`, and for a function the padding
    /// the linker filled with traps or `nop`s up to whatever is next.
    room: u64,
    kind: SymbolKind,
    /// Where the file has its entry of the symbol table, or [`Defined::NO_ENTRY`] for what a
    /// splice of all of a program added ([`Baseline::spliced_whole`]).
    entry: u64,
}

impl Defined {
    const NO_ENTRY: u64 = u64::MAX;

    fn is(&self, other: &Defined) -> bool {
        (self.address, self.size, self.kind) == (other.address, other.size, other.kind)
    }
}

/// `.eh_frame_hdr`: the frames of the program sorted by the code they describe, which the
/// unwinder finds through a program header.
struct UnwindTable {
    segment: usize,
    address: u64,
    /// Where `.eh_frame` starts.
    frames: u64,
    /// Pairs of the start of a function and its frame, both relative to `address`.
    entries: Vec<u8>,
}

/// `.rela.dyn`, and where `.dynamic` says where it is and how long.
struct LoadRelocations {
    table: Vec<u8>,
    address_at: u64,
    size_at: u64,
    /// Whether `DT_RELR` has more of them, in a form that is not added to here.
    packed: bool,
}

impl Image {
    fn read(bytes: &[u8]) -> anyhow::Result<Image> {
        let file = ElfFile64::<Endianness>::parse(bytes)?;
        let endian = file.endian();
        let header = *file.elf_header();
        let machine =
            Machine::of(header.e_machine(endian)).filter(|_| endian == Endianness::Little);
        let machine = machine.context("an executable of an architecture nothing is added to")?;
        let pie = match header.e_type(endian) {
            elf::ET_DYN => true,
            elf::ET_EXEC => false,
            other => bail!("not an executable (type {other})"),
        };
        let segments = file.elf_program_headers().to_vec();

        let mut symbols = FxHashMap::<Box<[u8]>, Named>::default();
        let (table, _) = (file.section_by_name(".symtab").and_then(|it| it.file_range()))
            .context("the executable has no symbol table")?;
        let code = |section| {
            let section = file.section_by_index(section).ok()?;
            let object::SectionFlags::Elf { sh_flags } = section.flags() else { return None };
            (sh_flags & u64::from(elf::SHF_EXECINSTR) != 0).then_some(section)
        };
        // Where each symbol of code starts, and each section of it ends: what is between a
        // function's end and the next is the linker's padding, or code no symbol names.
        let mut starts: Vec<u64> = (file.symbols())
            .filter(|it| it.section_index().and_then(code).is_some())
            .map(|it| it.address())
            .chain(
                file.sections()
                    .filter(|it| code(it.index()).is_some())
                    .map(|it| it.address() + it.size()),
            )
            .collect();
        starts.sort_unstable();
        let padding: &[u8] = match machine {
            Machine::X86_64 => &[0xcc, 0x90],
            Machine::Aarch64 => &[],
        };
        let room = |address: u64, size: u64, section| -> u64 {
            let Some(section) = code(section) else { return size };
            let end = address + size;
            let next = starts[starts.partition_point(|it| *it < end)..].first().copied();
            let Some(next) = next.filter(|it| *it > end) else { return size };
            let gap = (section.data_range(end, next - end).ok().flatten()).unwrap_or_default();
            match gap.len() as u64 == next - end && gap.iter().all(|it| padding.contains(it)) {
                true => next - address,
                false => size,
            }
        };
        // For the harness: every function shorter than a jump hops, as one with none of the
        // linker's padding behind it has to.
        let hops = std::env::var_os("BLAZE_SPLICE_HOPS").is_some();
        for symbol in file.symbols() {
            // A global label of assembly, of no type and no size, which `object` counts as
            // no definition, is one to the linker.
            let label = symbol.kind() == SymbolKind::Unknown && symbol.is_global();
            let defines = symbol.is_definition() || label || symbol.kind() == SymbolKind::Tls;
            let Some(section) = symbol.section_index().filter(|_| defines) else { continue };
            let name = symbol.name_bytes()?;
            let (address, size) = (symbol.address(), symbol.size());
            let short = symbol.kind() == SymbolKind::Text && size < machine.jump() as u64;
            let defined = Defined {
                address,
                size,
                room: if short && !hops { room(address, size, section) } else { size },
                kind: symbol.kind(),
                entry: table + (symbol.index().0 * size_of::<elf::Sym64<Endianness>>()) as u64,
            };
            match symbols.get_mut(name) {
                None => _ = symbols.insert(name.into(), Named::One(defined)),
                Some(Named::One(known)) if known.is(&defined) => {}
                Some(known) => *known = Named::Many,
            }
        }

        let mut pads = Vec::new();
        if machine == Machine::X86_64 {
            let mut spans: Vec<(usize, u64, u64)> = (file.symbols())
                .filter_map(|it| {
                    let section = it.section_index().filter(|it| code(*it).is_some())?;
                    Some((section.0, it.address(), it.address() + it.size()))
                })
                .collect();
            spans.sort_unstable();
            for section in file.sections().filter(|it| code(it.index()).is_some()) {
                let Ok(data) = section.data() else { continue };
                let (index, start) = (section.index().0, section.address());
                let of = &spans[spans.partition_point(|it| it.0 < index)..];
                let of = &of[..of.partition_point(|it| it.0 == index)];
                let past = start + data.len() as u64;
                let (mut end, mut short) = (start, 0);
                for &(_, from, to) in of.iter().chain([&(index, past, past)]) {
                    let from = from.min(past);
                    // A function shorter than a jump may take the start of the run for its own.
                    let at = end + short;
                    if from > at {
                        let run = &data[(at - start) as usize..(from - start) as usize];
                        if run.len() >= machine.jump() && run.iter().all(|it| *it == 0xcc) {
                            pads.push((at, from - at));
                        }
                    }
                    if to >= end {
                        let size = to - from;
                        short = if size < machine.jump() as u64 {
                            machine.jump() as u64 - size
                        } else {
                            0
                        };
                        end = to;
                    }
                }
            }
            pads.sort_unstable();
        }

        let mut imports = FxHashMap::default();
        if let (Some(relocations), Some(dynamic)) =
            (file.dynamic_relocations(), file.dynamic_symbol_table())
        {
            for (slot, relocation) in relocations {
                let RelocationFlags::Elf { r_type } = relocation.flags() else { continue };
                let RelocationTarget::Symbol(symbol) = relocation.target() else { continue };
                if machine.imports(r_type) {
                    let name = dynamic.symbol_by_index(symbol)?.name_bytes()?;
                    imports.entry(name.into()).or_insert(slot);
                }
            }
        }

        let mut image = Image {
            len: bytes.len() as u64,
            id: file.build_id().ok().flatten().map(binary_id),
            machine,
            header,
            segments,
            pie,
            symbols,
            imports,
            unwind: UnwindTable { segment: 0, address: 0, frames: 0, entries: Vec::new() },
            relocations: None,
            described: debug::Described::read(&file)?,
            pads,
        };
        image.unwind = UnwindTable::read(&image, bytes)?;
        image.relocations = LoadRelocations::read(&image, bytes)?;
        ensure!(
            !image.lands_on_pads(&file)?,
            "an executable whose indirect branches have to land on `bti`"
        );
        Ok(image)
    }

    /// Whether the loader is told to fault an indirect branch that lands on anything but a
    /// `bti`: the jump written over the entry of a function is none.
    fn lands_on_pads(&self, file: &ElfFile64<'_, Endianness>) -> anyhow::Result<bool> {
        let e = Endianness::Little;
        if self.machine != Machine::Aarch64 {
            return Ok(false);
        }
        for segment in file.elf_program_headers() {
            let Some(mut notes) = segment.notes(e, file.data())? else { continue };
            while let Some(note) = notes.next()? {
                let Some(mut properties) = note.gnu_properties(e) else { continue };
                while let Some(property) = properties.next()? {
                    if property.pr_type() == elf::GNU_PROPERTY_AARCH64_FEATURE_1_AND
                        && property.data_u32(e)? & elf::GNU_PROPERTY_AARCH64_FEATURE_1_BTI != 0
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// How far the thread-local `offset` into the executable's block of them is from the
    /// thread pointer: on x86-64 the block ends where the pointer is, at a multiple of what
    /// it is aligned to.
    fn thread_offset(&self, offset: u64) -> anyhow::Result<i32> {
        let e = Endianness::Little;
        ensure!(self.machine == Machine::X86_64, "a thread-local of an AArch64 executable");
        let block = self.segments_of(elf::PT_TLS).next().context("no block of thread-locals")?;
        let (start, size) = (block.p_vaddr(e), block.p_memsz(e));
        let align = block.p_align(e).max(1);
        let below = size + (start.wrapping_neg().wrapping_sub(size) & (align - 1));
        i32::try_from(offset as i64 - below as i64).context("a block of thread-locals of 2 GB")
    }

    fn segments_of(&self, kind: u32) -> impl Iterator<Item = &elf::ProgramHeader64<Endianness>> {
        self.segments.iter().filter(move |it| it.p_type(Endianness::Little) == kind)
    }

    /// Whether a function can be had to go on elsewhere: by a jump over its entry, or a hop.
    fn jumpable(&self, old: &Defined) -> bool {
        old.room >= self.machine.jump() as u64 || self.hop(old, &FxHashSet::default()).is_some()
    }

    /// Where a function of x86-64 too short for a `jmp` to its replacement has a `jmp` written
    /// for it: in a run of traps a 2-byte `jmp` from its entry reaches, at a place none of
    /// `taken` is.
    fn hop(&self, old: &Defined, taken: &FxHashSet<u64>) -> Option<u64> {
        let jump = self.machine.jump() as u64;
        if self.machine != Machine::X86_64 || old.size < 2 {
            return None;
        }
        let (low, high) = ((old.address + 2).saturating_sub(128), old.address + 2 + 127);
        let first = self.pads.partition_point(|&(at, len)| at + len < low + jump);
        let pads = self.pads[first..].iter().take_while(|it| it.0 <= high);
        pads.flat_map(|&(at, len)| (0..len / jump).map(move |it| at + it * jump))
            .find(|&it| (low..=high).contains(&it) && !taken.contains(&it))
    }

    /// Where in the file the linker put what is loaded at `address`.
    fn offset_of(&self, address: u64) -> anyhow::Result<u64> {
        let e = Endianness::Little;
        self.segments_of(elf::PT_LOAD)
            .find(|it| (it.p_vaddr(e)..it.p_vaddr(e) + it.p_filesz(e)).contains(&address))
            .map(|it| it.p_offset(e) + (address - it.p_vaddr(e)))
            .with_context(|| format!("nothing of the file is loaded at {address:#x}"))
    }

    fn bytes_at<'a>(&self, bytes: &'a [u8], address: u64, len: u64) -> anyhow::Result<&'a [u8]> {
        let start = self.offset_of(address)? as usize;
        bytes.get(start..start + len as usize).context("the file ends early")
    }

    /// The first address past everything the program loads.
    fn end(&self) -> u64 {
        let e = Endianness::Little;
        self.segments_of(elf::PT_LOAD).map(|it| it.p_vaddr(e) + it.p_memsz(e)).max().unwrap_or(0)
    }

    /// Where a splice adds to the file and to memory, at `offset` and `address` or past
    /// them: as far from each other as the linker's frames are in the two. A profiler that
    /// unwinds by the file finds the frames from the unwind table by how far they are from
    /// it in memory, so the table a splice writes has to be that far from them in the file.
    fn behind(&self, offset: u64, address: u64) -> (u64, u64) {
        let e = Endianness::Little;
        let frames = self.unwind.frames;
        let holds = |it: &&elf::ProgramHeader64<Endianness>| {
            (it.p_vaddr(e)..it.p_vaddr(e) + it.p_filesz(e)).contains(&frames)
        };
        let ahead = (self.segments_of(elf::PT_LOAD).find(holds))
            .and_then(|it| it.p_vaddr(e).checked_sub(it.p_offset(e)));
        match ahead {
            Some(ahead) => {
                let address = address.max(offset + ahead);
                (address - ahead, address)
            }
            None => (offset, address),
        }
    }

    /// Where a splice that adds `len` bytes adds them to the file and to memory. Behind
    /// the file, and as far from it in memory as the linker's frames are ([`Image::behind`]),
    /// where that is close enough: the patch's code and the program's reach each other, and
    /// the unwind table reaches every frame, by four-byte distances. A program whose file is
    /// gigabytes of what a debugger reads, behind a few hundred megabytes loaded, would have
    /// the patch too far from its code that way, and the patch is then right behind what the
    /// program loads instead. That costs perf, which finds a frame's rules as far into the
    /// file as the frame's mapping says, the rules of the patch's functions.
    fn placement(&self, len: u64) -> (u64, u64) {
        let e = Endianness::Little;
        let page = self.page();
        let (offset, address) =
            self.behind(self.len.next_multiple_of(page), self.end().next_multiple_of(page));
        let start = self.segments_of(elf::PT_LOAD).map(|it| it.p_vaddr(e)).min().unwrap_or(0);
        if address + len - start <= i32::MAX as u64 {
            return (offset, address);
        }
        (self.len.next_multiple_of(page), self.end().next_multiple_of(page))
    }

    fn page(&self) -> u64 {
        let e = Endianness::Little;
        self.segments_of(elf::PT_LOAD).map(|it| it.p_align(e)).max().unwrap_or(1).max(1)
    }

    /// Where a splice writes what a debugger reads, whatever the patch: behind what a splice
    /// adds of any usual size. The file has nothing in between.
    fn tail(&self) -> u64 {
        const ROOM: u64 = 64 << 20;
        let page = self.page();
        let (offset, _) =
            self.behind(self.len.next_multiple_of(page), self.end().next_multiple_of(page));
        let tables =
            self.unwind.len_with(0) + self.relocations.as_ref().map_or(0, |it| it.len_with(0));
        offset + tables as u64 + ROOM
    }
}

impl UnwindTable {
    /// The encodings every linker writes the table in: a 4-byte count, and pairs of 4-byte
    /// offsets from the table's start.
    const ENCODINGS: [u8; 4] =
        [1, eh::RELATIVE | eh::SIGNED_4, eh::UNSIGNED_4, eh::TABLE_RELATIVE | eh::SIGNED_4];
    const HEADER: usize = 12;
    const ENTRY: usize = 8;

    fn read(image: &Image, bytes: &[u8]) -> anyhow::Result<UnwindTable> {
        let e = Endianness::Little;
        let (segment, header) = image
            .segments
            .iter()
            .enumerate()
            .find(|it| it.1.p_type(e) == elf::PT_GNU_EH_FRAME)
            .context("the executable has no unwind table")?;
        let address = header.p_vaddr(e);
        let table = image.bytes_at(bytes, address, header.p_filesz(e))?;
        ensure!(
            table.len() >= Self::HEADER && table[..4] == Self::ENCODINGS,
            "an unwind table in encodings we don't write"
        );
        let frames = (address + 4).wrapping_add_signed(i64::from(read_i32(&table[4..])));
        let count = u32::from_le_bytes(table[8..12].try_into()?) as usize;
        let entries = table
            .get(Self::HEADER..Self::HEADER + count * Self::ENTRY)
            .context("the unwind table ends early")?
            .to_vec();
        Ok(UnwindTable { segment, address, frames, entries })
    }

    fn len_with(&self, added: usize) -> usize {
        Self::HEADER + self.entries.len() + added * Self::ENTRY
    }

    /// The table as it is at `address`, with `added` frames given by the address of their
    /// code and their own. All of them are of code behind the program's.
    fn moved(&self, address: u64, mut added: Vec<(u64, u64)>) -> anyhow::Result<Vec<u8>> {
        added.sort_unstable();
        let mut table = Vec::with_capacity(self.len_with(added.len()));
        table.extend_from_slice(&Self::ENCODINGS);
        table.extend_from_slice(&relative(self.frames, address + 4)?.to_le_bytes());
        let count = self.entries.len() / Self::ENTRY + added.len();
        table.extend_from_slice(&u32::try_from(count)?.to_le_bytes());
        let shift = relative(self.address, address)?;
        for &field in self.entries.as_chunks().0 {
            let field =
                i32::from_le_bytes(field).checked_add(shift).context("the program is too large")?;
            table.extend_from_slice(&field.to_le_bytes());
        }
        for (code, frame) in added {
            table.extend_from_slice(&relative(code, address)?.to_le_bytes());
            table.extend_from_slice(&relative(frame, address)?.to_le_bytes());
        }
        Ok(table)
    }
}

impl LoadRelocations {
    const ENTRY: usize = size_of::<elf::Rela64<Endianness>>();
    /// The tag of the packed relative relocations, which `object` has no name for.
    const DT_RELR: u32 = 36;

    fn read(image: &Image, bytes: &[u8]) -> anyhow::Result<Option<LoadRelocations>> {
        let e = Endianness::Little;
        let Some(dynamic) = image.segments_of(elf::PT_DYNAMIC).next() else { return Ok(None) };
        let start = dynamic.p_offset(e);
        let entries = bytes
            .get(start as usize..(start + dynamic.p_filesz(e)) as usize)
            .context("the file ends early")?;
        let (entries, _) = object::slice_from_bytes::<elf::Dyn64<Endianness>>(
            entries,
            entries.len() / size_of::<elf::Dyn64<Endianness>>(),
        )
        .map_err(|()| anyhow::anyhow!("a misaligned dynamic section"))?;
        let entry = |tag: u32| {
            let at = entries.iter().position(|it| it.d_tag.get(e) == u64::from(tag))?;
            let value_at = start + (at * size_of::<elf::Dyn64<Endianness>>()) as u64 + 8;
            Some((entries[at].d_val.get(e), value_at))
        };
        let (Some((address, address_at)), Some((size, size_at))) =
            (entry(elf::DT_RELA), entry(elf::DT_RELASZ))
        else {
            return Ok(None);
        };
        let table = image.bytes_at(bytes, address, size)?.to_vec();
        let packed = entry(Self::DT_RELR).is_some();
        Ok(Some(LoadRelocations { table, address_at, size_at, packed }))
    }

    fn len_with(&self, added: usize) -> usize {
        self.table.len() + added * Self::ENTRY
    }

    fn rebases(&self, relative: u32) -> Vec<u64> {
        let e = Endianness::Little;
        let Ok((entries, _)) = object::slice_from_bytes::<elf::Rela64<Endianness>>(
            &self.table,
            self.table.len() / Self::ENTRY,
        ) else {
            return Vec::new();
        };
        entries
            .iter()
            .filter(|it| it.r_info.get(e) == u64::from(relative))
            .map(|it| it.r_offset.get(e))
            .collect()
    }

    /// Makes the entries of `table` for the addresses that `statics` stored those for what
    /// they store now, which `restored` has by where. An address no longer stored has its
    /// entry fix up `spare` instead; the entries a new one needs are handed back.
    fn restore(
        table: &mut [u8],
        mut statics: Vec<Defined>,
        mut restored: FxHashMap<u64, u64>,
        spare: u64,
        relative: u32,
    ) -> anyhow::Result<Vec<elf::Rela64<Endianness>>> {
        let e = Endianness::Little;
        if !statics.is_empty() {
            statics.sort_unstable_by_key(|it| it.address);
            let entries = object::slice_from_all_bytes_mut::<elf::Rela64<Endianness>>(table)
                .map_err(|()| anyhow::anyhow!("misaligned load-time relocations"))?;
            for entry in entries {
                let at = entry.r_offset.get(e);
                let before = statics.partition_point(|it| it.address <= at);
                let Some(old) = before.checked_sub(1).map(|it| statics[it]) else { continue };
                if at >= old.address + old.size {
                    continue;
                }
                ensure!(
                    entry.r_info.get(e) == u64::from(relative),
                    "a static that stored the address of an import"
                );
                match restored.remove(&at) {
                    Some(value) => entry.r_addend.set(e, value as i64),
                    None => {
                        entry.r_offset.set(e, spare);
                        entry.r_addend.set(e, 0);
                    }
                }
            }
        }
        Ok(restored
            .into_iter()
            .map(|(at, value)| elf::Rela64 {
                r_offset: object::U64::new(e, at),
                r_info: object::U64::new(e, u64::from(relative)),
                r_addend: object::I64::new(e, value as i64),
            })
            .collect())
    }
}

/// How `.eh_frame` and `.eh_frame_hdr` say an address is written (`DW_EH_PE_*`).
mod eh {
    pub(super) const ABSOLUTE: u8 = 0x00;
    pub(super) const UNSIGNED_4: u8 = 0x03;
    pub(super) const SIGNED_4: u8 = 0x0b;
    /// From where the address is written.
    pub(super) const RELATIVE: u8 = 0x10;
    /// From the start of the search table.
    pub(super) const TABLE_RELATIVE: u8 = 0x30;
}

/// What the frames of `.eh_frame` have in common, which each refers to.
struct CommonEntry;

impl CommonEntry {
    /// How the frames of the entry at `at` write where their code starts.
    fn pointer_encoding(section: &[u8], at: usize) -> anyhow::Result<u8> {
        let mut rest = section.get(at + 8..).context("a frame of no common entry")?;
        let mut byte = || -> anyhow::Result<u8> {
            let (&first, others) = rest.split_first().context("a common entry that ends early")?;
            rest = others;
            Ok(first)
        };
        let version = byte()?;
        ensure!(version == 1 || version == 3, "a common entry of version {version}");
        let mut augmentation = Vec::new();
        loop {
            match byte()? {
                0 => break,
                letter => augmentation.push(letter),
            }
        }
        let mut leb = || -> anyhow::Result<()> {
            while byte()? & 0x80 != 0 {}
            Ok(())
        };
        // The alignment of code and of data, the register of the return address, and the
        // length of what the augmentation adds.
        for _ in 0..4 {
            leb()?;
        }
        ensure!(augmentation.first() == Some(&b'z'), "a common entry without augmentation");
        for letter in &augmentation[1..] {
            match letter {
                b'R' => return byte(),
                b'L' => _ = byte()?,
                b'S' | b'B' => {}
                // The personality routine: an encoding and then an address written in it.
                b'P' => {
                    let len = match byte()? & 0x0f {
                        0x00 | 0x04 | 0x0c => 8,
                        0x03 | 0x0b => 4,
                        0x02 | 0x0a => 2,
                        other => bail!("a personality routine encoded as {other:#x}"),
                    };
                    for _ in 0..len {
                        byte()?;
                    }
                }
                other => bail!("a common entry augmented with {:?}", *other as char),
            }
        }
        Ok(eh::ABSOLUTE)
    }
}

/// A patch object, and what of the executable it needs.
struct Patch<'a> {
    file: ElfFile64<'a, Endianness>,
    /// The sections that are loaded, by the segment they go to.
    sections: [Vec<SectionIndex>; 3],
    /// What each symbol a relocation refers to stands for.
    targets: FxHashMap<SymbolIndex, Target>,
    /// The symbols the code reaches through an address slot of ours, in the order of the
    /// slots.
    slots: Vec<SymbolIndex>,
    /// What the code calls directly and may not reach, in the order of their stubs: the
    /// imports, and where a call reaches only so far, what the executable defines.
    stubs: Vec<SymbolIndex>,
    /// How many addresses the data stores, each of which the loader has to fix up.
    stored: usize,
    frames: usize,
    /// The functions and statics it defines, with what the executable has under the name.
    replaced: Vec<Replaced<'a>>,
    /// The statics among them that go where the executable has them, by where the patch does.
    in_place: FxHashMap<SectionIndex, Vec<InPlace>>,
}

#[derive(Clone, Copy)]
enum Segment {
    Read = 0,
    Code = 1,
    Data = 2,
}

impl Segment {
    const ALL: [Segment; 3] = [Segment::Read, Segment::Code, Segment::Data];

    fn flags(self) -> u32 {
        match self {
            Segment::Read => elf::PF_R,
            Segment::Code => elf::PF_R | elf::PF_X,
            Segment::Data => elf::PF_R | elf::PF_W,
        }
    }
}

#[derive(Clone, Copy)]
enum Target {
    /// At an offset in a section of the patch.
    Ours(SectionIndex, u64),
    /// A section of the patch, where what is meant is as far in as the relocation adds.
    Section(SectionIndex),
    /// At an address of the executable.
    Theirs(u64),
    /// In another library: the loader writes its address to the slot.
    Import(u64),
    /// A thread-local of the executable, this far from the thread pointer.
    ThreadLocal(i32),
}

struct Replaced<'a> {
    name: &'a [u8],
    section: SectionIndex,
    offset: u64,
    size: u64,
    /// `None` for what the executable does not have.
    old: Option<Defined>,
}

/// A static of the patch that is written over the executable's.
#[derive(Clone, Copy)]
struct InPlace {
    /// Where its section has it.
    offset: u64,
    size: u64,
    old: Defined,
}

impl InPlace {
    fn holds(&self, offset: u64) -> bool {
        (self.offset..self.offset + self.size).contains(&offset)
    }

    /// Which of `all`, by where their section has them, holds `offset`.
    fn holding(all: &[InPlace], offset: u64) -> Option<usize> {
        let at = all.partition_point(|it| it.offset + it.size <= offset);
        all.get(at).filter(|it| it.holds(offset)).map(|_| at)
    }
}

/// How a relocation of the patch gets its value.
enum Fixup {
    /// The address itself.
    Absolute,
    /// Where it is from the code: a distance, or the parts of one.
    Relative,
    /// Where a slot that holds the address is from the code.
    ThroughSlot,
    /// The address of a thread-local, as `__tls_get_addr` gives it: the code that calls it
    /// is written over with code that adds to the thread pointer.
    ThreadLocal,
}

impl Fixup {
    fn of(relocation: &object::Relocation, machine: Machine) -> anyhow::Result<(Fixup, Field)> {
        let RelocationFlags::Elf { r_type } = relocation.flags() else {
            bail!("a relocation that is not ELF's")
        };
        machine.fixup(r_type)
    }

    /// Where the field of a call to `__tls_get_addr` is in a section, for each thread-local
    /// it reaches: the call goes, with the code that makes it.
    fn thread_calls(
        relocations: impl Iterator<Item = (u64, object::Relocation)>,
        machine: Machine,
    ) -> anyhow::Result<FxHashSet<u64>> {
        let mut calls = FxHashSet::default();
        for (at, relocation) in relocations {
            if let (Fixup::ThreadLocal, _) = Fixup::of(&relocation, machine)? {
                calls.insert(at + 8);
            }
        }
        Ok(calls)
    }

    /// The code `__tls_get_addr` is called by for an `R_X86_64_TLSGD`, whose field is the
    /// 4 bytes after `GENERAL.0`: `data16 lea sym@tlsgd(%rip), %rdi`, then
    /// `data16 data16 rex.W call __tls_get_addr`.
    const GENERAL: ([u8; 4], [u8; 4]) = ([0x66, 0x48, 0x8d, 0x3d], [0x66, 0x66, 0x48, 0xe8]);

    /// What is written over `GENERAL` for a thread-local `offset` from the thread pointer:
    /// `mov %fs:0, %rax`, then `lea offset(%rax), %rax`.
    fn local(offset: i32) -> [u8; 16] {
        let mut code = [0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0x48, 0x8d, 0x80, 0, 0, 0, 0];
        code[12..].copy_from_slice(&offset.to_le_bytes());
        code
    }
}

impl<'a> Patch<'a> {
    const SLOT: usize = 8;
    /// What a table of the executable is given to grow into before what follows it moves.
    const ROOM: u64 = 1 << 20;

    fn read(bytes: &'a [u8], image: &Image) -> anyhow::Result<Patch<'a>> {
        let file = ElfFile64::<Endianness>::parse(bytes)?;
        let mut patch = Patch {
            file,
            sections: Default::default(),
            targets: FxHashMap::default(),
            slots: Vec::new(),
            stubs: Vec::new(),
            stored: 0,
            frames: 0,
            replaced: Vec::new(),
            in_place: FxHashMap::default(),
        };
        for section in patch.file.sections() {
            let object::SectionFlags::Elf { sh_flags } = section.flags() else { continue };
            if sh_flags & u64::from(elf::SHF_ALLOC) == 0 {
                continue;
            }
            ensure!(sh_flags & u64::from(elf::SHF_TLS) == 0, "the patch has thread-local data");
            let segment = if sh_flags & u64::from(elf::SHF_EXECINSTR) != 0 {
                Segment::Code
            } else if sh_flags & u64::from(elf::SHF_WRITE) != 0 {
                Segment::Data
            } else {
                Segment::Read
            };
            patch.sections[segment as usize].push(section.index());
            if section.name_bytes()? == b".eh_frame" {
                patch.frames += Self::frames_in(section.data()?, 0)?.len();
            }
        }
        let loaded: FxHashSet<SectionIndex> = patch.sections.iter().flatten().copied().collect();
        let code: FxHashSet<SectionIndex> =
            patch.sections[Segment::Code as usize].iter().copied().collect();

        for symbol in patch.file.symbols() {
            if !symbol.is_definition() || symbol.is_local() {
                continue;
            }
            let name = symbol.name_bytes()?;
            let section = symbol.section_index().context("a symbol in no section")?;
            let old = match image.symbols.get(name) {
                None => None,
                // The executable's is the same static with the same value: it is kept.
                Some(_) if is_lasting(name) => None,
                // One that says what a patch left behind is of that patch, which the executable
                // was linked with, and says nothing of this one's.
                Some(_) if left_behind(name).is_some() => None,
                Some(Named::One(old))
                    if old.kind == SymbolKind::Text && code.contains(&section) =>
                {
                    ensure!(
                        image.jumpable(old),
                        "{} is too short to jump from",
                        String::from_utf8_lossy(name)
                    );
                    Some(*old)
                }
                Some(Named::One(old))
                    if old.kind == SymbolKind::Data && !code.contains(&section) =>
                {
                    let new = InPlace { offset: symbol.address(), size: symbol.size(), old: *old };
                    patch.fits(&new, section, image).with_context(|| {
                        format!("the static {} has another value", String::from_utf8_lossy(name))
                    })?;
                    patch.in_place.entry(section).or_default().push(new);
                    Some(*old)
                }
                Some(_) => bail!(
                    "the patch replaces {}, which is not one function or static",
                    String::from_utf8_lossy(name)
                ),
            };
            patch.replaced.push(Replaced {
                name,
                section,
                offset: symbol.address(),
                size: symbol.size(),
                old,
            });
        }
        let named: Vec<SymbolIndex> = (patch.described())
            .flat_map(|section| section.relocations())
            .filter_map(|(_, relocation)| match relocation.target() {
                RelocationTarget::Symbol(index) => Some(index),
                _ => None,
            })
            .collect();
        for index in named {
            let section = patch.file.symbol_by_index(index)?.section_index();
            if section.is_some_and(|it| loaded.contains(&it)) && !patch.targets.contains_key(&index)
            {
                let target = patch.target(index, image, &loaded)?;
                patch.targets.insert(index, target);
            }
        }
        let (mut slots, mut stubs) = (FxHashSet::default(), FxHashSet::default());
        for section in patch.file.sections().filter(|it| loaded.contains(&it.index())) {
            let calls = Fixup::thread_calls(section.relocations(), image.machine)?;
            for (at, relocation) in section.relocations() {
                let RelocationTarget::Symbol(index) = relocation.target() else {
                    bail!("a relocation against no symbol")
                };
                if calls.contains(&at) {
                    let called = patch.file.symbol_by_index(index)?.name_bytes()?;
                    ensure!(called == b"__tls_get_addr", "a thread-local reached another way");
                    continue;
                }
                let target = match patch.targets.get(&index) {
                    Some(target) => *target,
                    None => {
                        let target = patch.target(index, image, &loaded)?;
                        patch.targets.insert(index, target);
                        target
                    }
                };
                match (Fixup::of(&relocation, image.machine)?, target) {
                    ((Fixup::ThreadLocal, _), Target::ThreadLocal(_)) => {
                        ensure!(relocation.addend() == -4, "a thread-local reached another way");
                    }
                    ((Fixup::ThreadLocal, _), _) | (_, Target::ThreadLocal(_)) => {
                        bail!("a thread-local reached another way")
                    }
                    ((Fixup::Absolute, _), Target::Import(_)) => {
                        bail!("data that stores the address of an import")
                    }
                    ((Fixup::Absolute, _), _) => patch.stored += 1,
                    ((Fixup::Relative, _), Target::Import(_))
                    | ((Fixup::Relative, Field::Branch), Target::Theirs(_)) => {
                        if stubs.insert(index) {
                            patch.stubs.push(index);
                        }
                    }
                    ((Fixup::ThroughSlot, _), Target::Import(_)) => {}
                    ((Fixup::ThroughSlot, _), _) => {
                        if slots.insert(index) {
                            patch.slots.push(index);
                        }
                    }
                    ((Fixup::Relative, _), _) => {}
                }
            }
        }

        for statics in patch.in_place.values_mut() {
            statics.sort_unstable_by_key(|it| it.offset);
        }
        patch.keep_lasting(|name| match image.symbols.get(name) {
            Some(Named::One(Defined { kind: SymbolKind::Data, address, .. })) => Some(*address),
            _ => None,
        })?;
        Ok(patch)
    }

    /// Has what refers to a static the patch defines anew ([`is_lasting`]) refer to the copy
    /// `kept` finds of it instead, where it finds one: the executable's, or one an earlier
    /// patch gave the running program. The patch's own copy is left unused.
    fn keep_lasting(&mut self, kept: impl Fn(&[u8]) -> Option<u64>) -> anyhow::Result<()> {
        let mut sections = FxHashSet::default();
        for (&index, target) in &mut self.targets {
            let Target::Ours(section, _) = *target else { continue };
            let name = self.file.symbol_by_index(index)?.name_bytes()?;
            if !is_lasting(name) {
                continue;
            }
            if let Some(address) = kept(name) {
                *target = Target::Theirs(address);
                sections.insert(section);
            }
        }
        // What is reached through its section and not its name would be the patch's copy.
        let through_section = self
            .targets
            .values()
            .any(|it| matches!(it, Target::Section(at) if sections.contains(at)));
        ensure!(!through_section, "a static of the patch's own is reached through its section");
        Ok(())
    }

    /// Whether a static can go where the executable has the one it replaces: what refers to
    /// it there is not looked for.
    fn fits(&self, new: &InPlace, section: SectionIndex, image: &Image) -> anyhow::Result<()> {
        let e = Endianness::Little;
        let old = &new.old;
        ensure!(new.size <= old.size, "that is larger: {} bytes after {}", new.size, old.size);
        let section = self.file.section_by_index(section)?;
        // What a type is aligned to divides its size, and where its section has it.
        let align = (new.offset | new.size | section.align().max(1)).isolate_lowest_one();
        ensure!(old.address.is_multiple_of(align), "that may be aligned to {align}");
        let segment = image
            .segments_of(elf::PT_LOAD)
            .find(|it| {
                let start = it.p_vaddr(e);
                start <= old.address && old.address + new.size <= start + it.p_filesz(e)
            })
            .context("that the file has no bytes for")?;
        let object::SectionFlags::Elf { sh_flags } = section.flags() else { bail!("no flags") };
        ensure!(
            sh_flags & u64::from(elf::SHF_WRITE) == 0 || segment.p_flags(e) & elf::PF_W != 0,
            "that is written to, where the old one is not"
        );
        Ok(())
    }

    fn target(
        &self,
        index: SymbolIndex,
        image: &Image,
        loaded: &FxHashSet<SectionIndex>,
    ) -> anyhow::Result<Target> {
        let symbol = self.file.symbol_by_index(index)?;
        if let Some(section) = symbol.section_index() {
            ensure!(loaded.contains(&section), "a reference to a section that is not loaded");
            return Ok(match symbol.kind() {
                SymbolKind::Section => Target::Section(section),
                _ => Target::Ours(section, symbol.address()),
            });
        }
        let name = symbol.name_bytes()?;
        let shown = || String::from_utf8_lossy(name);
        ensure!(symbol.is_undefined(), "{} is neither defined nor undefined", shown());
        match image.symbols.get(name) {
            Some(Named::One(Defined { kind: SymbolKind::Tls, address, .. })) => {
                Ok(Target::ThreadLocal(image.thread_offset(*address)?))
            }
            Some(Named::One(defined)) => Ok(Target::Theirs(defined.address)),
            Some(Named::Many) => bail!("{} names several things in the executable", shown()),
            None => match image.imports.get(name) {
                Some(slot) => Ok(Target::Import(*slot)),
                None => bail!("{} is not in the executable", shown()),
            },
        }
    }

    /// The frames in an `.eh_frame` section loaded at `address`: where the code of each
    /// starts, and where the frame is.
    fn frames_in(section: &[u8], address: u64) -> anyhow::Result<Vec<(u64, u64)>> {
        let early = "an unwind table that ends early";
        let u32_at = |at: usize| -> anyhow::Result<u32> {
            let bytes = section.get(at..at + 4).context(early)?;
            Ok(u32::from_le_bytes(bytes.try_into()?))
        };
        let mut found = Vec::new();
        let mut at = 0;
        while at < section.len() {
            let len = u32_at(at)? as usize;
            if len == 0 {
                break;
            }
            ensure!(len != u32::MAX as usize, "an unwind table in the 64-bit format");
            // A frame says how far back its common entry is; a common entry says zero.
            let back = u32_at(at + 4)? as usize;
            if back != 0 {
                let common = (at + 4).checked_sub(back).context("a frame of no common entry")?;
                let start = at + 8;
                let begins = match CommonEntry::pointer_encoding(section, common)? {
                    eh::ABSOLUTE => {
                        let bytes = section.get(start..start + 8).context(early)?;
                        u64::from_le_bytes(bytes.try_into()?)
                    }
                    encoding if encoding == eh::RELATIVE | eh::SIGNED_4 => (address + start as u64)
                        .wrapping_add_signed(i64::from(u32_at(start)? as i32)),
                    other => bail!("frames whose addresses are encoded as {other:#x}"),
                };
                found.push((begins, address + at as u64));
            }
            at += 4 + len;
        }
        Ok(found)
    }

    /// Lays the patch out behind the executable and gives everything in it its address.
    fn place(&self, image: &Image, copies: &debug::Copies) -> anyhow::Result<Addition> {
        self.place_at(image, copies, None)
    }

    fn place_live_simple(
        &self,
        image: &Image,
        copies: &debug::Copies,
        originals: &FxHashMap<u64, Vec<u8>>,
        base: u64,
        lasting: &mut Lasting,
    ) -> anyhow::Result<(blaze_live::format::Code, Vec<(u64, u64)>)> {
        let own = self.lasting_own()?;
        let addition = self.place_at(image, copies, Some(base))?;
        let copy = |name: &[u8]| {
            let found = addition.defined.iter().find(|it| &*it.0 == name);
            found.map(|it| it.1.address - base)
        };
        let first =
            lasting.first_values(&own, copy, &addition.appended, base, &addition.live_pointers)?;
        lasting.note(self.lasting_in(&addition.defined));
        let mut all = addition.appended;
        let data_at = addition.live_data_at as usize;
        let data = all.split_off(data_at);
        let mut text = all;
        text.resize(text.len().next_multiple_of(image.page() as usize), 0);
        let mut writes = Vec::new();
        let mut moved = Vec::new();
        for replaced in &self.replaced {
            let Some(old) = replaced.old.filter(|it| it.kind == SymbolKind::Text) else { continue };
            let offset = image.offset_of(old.address)?;
            let bytes = addition
                .overwritten
                .iter()
                .find(|(at, _)| *at == offset)
                .map(|(_, bytes)| bytes.clone())
                .context("the function jump is missing")?;
            moved.push((base, old.address));
            writes.push(blaze_live::format::Write {
                address: old.address,
                bytes,
                pointers: Vec::new(),
            });
        }
        let redirected = writes.len() as u64;
        let rebases = image
            .relocations
            .as_ref()
            .map(|it| it.rebases(image.machine.relative()))
            .unwrap_or_default();
        for old in self.in_place.values().flatten() {
            let offset = image.offset_of(old.old.address)?;
            let bytes = addition
                .overwritten
                .iter()
                .find(|(at, _)| *at == offset)
                .map(|(_, bytes)| bytes.clone())
                .context("the static write is missing")?;
            let after = rebases.partition_point(|it| *it < old.old.address);
            let pointers = rebases[after..]
                .iter()
                .take_while(|it| **it < old.old.address + bytes.len() as u64)
                .map(|it| it - old.old.address)
                .collect();
            writes.push(blaze_live::format::Write { address: old.old.address, bytes, pointers });
        }
        let statics = writes.len() as u64 - redirected;
        let written: FxHashSet<u64> = writes.iter().map(|it| it.address).collect();
        for (&address, bytes) in originals {
            if !written.contains(&address) {
                writes.push(blaze_live::format::Write {
                    address,
                    bytes: bytes.clone(),
                    pointers: Vec::new(),
                });
            }
        }
        let code = blaze_live::format::Code {
            base,
            text,
            data,
            frames: addition.live_frames,
            pointers: addition.live_pointers,
            writes,
            redirected,
            statics,
            lasting: first,
        };
        Ok((code, moved))
    }

    /// Where each static the patch defines anew is in the program: its own copy, or the one
    /// [`Self::keep_lasting`] kept.
    fn lasting_in(&self, defined: &[(Box<[u8]>, Defined)]) -> Vec<(Box<[u8]>, u64)> {
        let kept: FxHashMap<&[u8], u64> = (self.targets.iter())
            .filter_map(|(&index, target)| match target {
                Target::Theirs(address) => {
                    let name = self.file.symbol_by_index(index).ok()?.name_bytes().ok()?;
                    is_lasting(name).then_some((name, *address))
                }
                _ => None,
            })
            .collect();
        (defined.iter())
            .filter(|(name, _)| is_lasting(name))
            .map(|(name, it)| (name.clone(), kept.get(&**name).copied().unwrap_or(it.address)))
            .collect()
    }

    /// The statics the patch defines anew, with what they take.
    fn lasting_own(&self) -> anyhow::Result<Vec<Own>> {
        let mut own = Vec::new();
        for replaced in self.replaced.iter().filter(|it| is_lasting(it.name)) {
            let section = self.file.section_by_index(replaced.section)?;
            // What a type is aligned to divides its size, and where its section has it.
            let align = (replaced.offset | replaced.size | section.align().max(1))
                .isolate_lowest_one()
                .min(section.align().max(1));
            own.push(Own { name: replaced.name.into(), size: replaced.size, align });
        }
        Ok(own)
    }

    fn place_at(
        &self,
        image: &Image,
        copies: &debug::Copies,
        live_base: Option<u64>,
    ) -> anyhow::Result<Addition> {
        let e = Endianness::Little;
        let code: FxHashSet<SectionIndex> =
            self.sections[Segment::Code as usize].iter().copied().collect();
        let page = image.page();
        let machine = image.machine;
        let (stub_len, stub_align) = machine.stub();
        let relocations = image.relocations.as_ref().filter(|_| image.pie);
        ensure!(!image.pie || relocations.is_some(), "an executable the loader fixes nothing in");
        let fixed_up = if image.pie { self.stored + self.slots.len() } else { 0 };

        // Where everything goes, as offsets from `base`.
        let mut end = 0u64;
        let mut reserve = |len: usize, align: u64| {
            let start = end.next_multiple_of(align);
            end = start + len as u64;
            start
        };
        let mut placed = FxHashMap::default();
        let mut extents = [(0, 0); 3];
        let headers_len = (image.segments.len() + Segment::ALL.len() + 1)
            * size_of::<elf::ProgramHeader64<Endianness>>();
        let (mut unwind_at, mut relocations_at) = (0, 0);
        let (mut stubs_at, mut slots_at, mut spare_at) = (0, 0, 0);
        for segment in Segment::ALL {
            let start = reserve(0, page);
            // The two tables of the executable first, each with room behind it, so that
            // they are where the last splice has them whatever the patch is.
            if let Segment::Read = segment {
                unwind_at = reserve(image.unwind.len_with(self.frames), 4);
                relocations_at =
                    reserve(relocations.map_or(0, |it| it.len_with(fixed_up)), Self::ROOM);
                reserve(0, Self::ROOM);
            }
            for &index in &self.sections[segment as usize] {
                let section = self.file.section_by_index(index)?;
                placed.insert(index, reserve(section.size() as usize, section.align().max(1)));
            }
            match segment {
                Segment::Read => {}
                Segment::Code => stubs_at = reserve(self.stubs.len() * stub_len, stub_align),
                Segment::Data => {
                    slots_at = reserve(self.slots.len() * Self::SLOT, 8);
                    spare_at = reserve(if self.in_place.is_empty() { 0 } else { Self::SLOT }, 8);
                }
            }
            extents[segment as usize] = (start, reserve(0, 1));
        }
        let (offset, base) = live_base.map_or_else(|| image.placement(end), |base| (0, base));
        // The headers go where their place in memory is their place in the file, further
        // by what the first segment is: past the other three in both.
        let first = image.segments_of(elf::PT_LOAD).next().context("nothing is loaded")?;
        let ahead = first.p_vaddr(e).checked_sub(first.p_offset(e));
        let ahead = ahead
            .filter(|it| it.is_multiple_of(page))
            .context("a first segment loaded before where the file has it")?;
        let headers_at =
            (offset + end).max((base + end).saturating_sub(ahead)).next_multiple_of(page) - offset;
        let headers_address = offset + headers_at + ahead;
        let mut appended = vec![0u8; end as usize];

        // A static written in place is where the executable has it: its section is added
        // as a whole, with a copy of it that nothing may use. Code says how far its field is
        // from what it means in what it adds, so a section alone does not tell which static.
        let address = |target: Target, added: i64, exact: bool| -> anyhow::Result<u64> {
            let (section, at) = match target {
                Target::Ours(section, at) => (section, at),
                Target::Section(section) => (section, 0),
                Target::Theirs(address) => return Ok(address.wrapping_add_signed(added)),
                Target::Import(slot) => return Ok(slot.wrapping_add_signed(added)),
                Target::ThreadLocal(_) => bail!("the address of a thread-local"),
            };
            let in_place = self.in_place.get(&section).map_or(&[][..], |it| it);
            let found = match target {
                Target::Section(_) if exact => {
                    InPlace::holding(in_place, added as u64).map(|it| (&in_place[it], added as u64))
                }
                Target::Section(_) => {
                    // The first that ends past it is the one it can be just ahead of.
                    let first =
                        in_place.partition_point(|it| (it.offset + it.size) as i64 <= added);
                    let near = in_place.get(first).is_some_and(|it| it.offset as i64 - 8 <= added);
                    ensure!(!near, "code that refers to a static through its section");
                    None
                }
                _ => InPlace::holding(in_place, at).map(|it| (&in_place[it], at)),
            };
            Ok(match (found, target) {
                (Some((it, at)), Target::Section(_)) => it.old.address + (at - it.offset),
                (Some((it, at)), _) => {
                    (it.old.address + (at - it.offset)).wrapping_add_signed(added)
                }
                (None, _) => (base + placed[&section] + at).wrapping_add_signed(added),
            })
        };
        let slot_of: FxHashMap<SymbolIndex, u64> = (self.slots.iter().copied())
            .zip((0..).map(|at| base + slots_at + at * Self::SLOT as u64))
            .collect();
        let stub_of: FxHashMap<SymbolIndex, u64> = (self.stubs.iter().copied())
            .zip((0..).map(|at| base + stubs_at + at * stub_len as u64))
            .collect();

        // The addresses stored in data, which the loader moves with the program.
        let mut fixed = Vec::with_capacity(fixed_up);
        let mut store = |appended: &mut [u8], at: u64, value: u64| {
            appended[(at - base) as usize..][..8].copy_from_slice(&value.to_le_bytes());
            if image.pie {
                fixed.push(elf::Rela64 {
                    r_offset: object::U64::new(e, at),
                    r_info: object::U64::new(e, u64::from(machine.relative())),
                    r_addend: object::I64::new(e, value as i64),
                });
            }
        };

        let mut overwritten = Vec::new();
        // The addresses the statics written in place store, by where.
        let mut restored = FxHashMap::default();
        let mut frames = Vec::with_capacity(self.frames);
        for section in self.file.sections() {
            let Some(&start) = placed.get(&section.index()) else { continue };
            let data = section.data()?;
            appended[start as usize..][..data.len()].copy_from_slice(data);
            let in_place = self.in_place.get(&section.index()).map_or(&[][..], |it| it);
            let code = code.contains(&section.index());
            let mut values: Vec<Vec<u8>> = in_place
                .iter()
                .map(|it| match data.get(it.offset as usize..(it.offset + it.size) as usize) {
                    Some(value) => value.to_vec(),
                    None => vec![0; it.size as usize],
                })
                .collect();
            let calls = Fixup::thread_calls(section.relocations(), machine)?;
            for (at, relocation) in section.relocations() {
                let RelocationTarget::Symbol(index) = relocation.target() else { continue };
                if calls.contains(&at) {
                    continue;
                }
                let target = self.targets[&index];
                if let Some(which) = InPlace::holding(in_place, at) {
                    let within = (at - in_place[which].offset) as usize;
                    let (Fixup::Absolute, _) = Fixup::of(&relocation, machine)? else {
                        bail!("a static that stores a distance")
                    };
                    let value = address(target, relocation.addend(), true)?;
                    let field = values[which].get_mut(within..within + 8);
                    field
                        .context("an address past its static")?
                        .copy_from_slice(&value.to_le_bytes());
                    restored.insert(in_place[which].old.address + within as u64, value);
                    continue;
                }
                let place = base + start + at;
                let (fixup, field) = Fixup::of(&relocation, machine)?;
                let to = match (fixup, target) {
                    (Fixup::ThreadLocal, Target::ThreadLocal(offset)) => {
                        let code = (place - base - 4) as usize;
                        let code = appended.get_mut(code..code + 16);
                        let code = code.context("a relocation past its section")?;
                        let (lea, call) = Fixup::GENERAL;
                        ensure!(
                            code[..4] == lea && code[8..12] == call,
                            "a thread-local reached by code of another shape"
                        );
                        code.copy_from_slice(&Fixup::local(offset));
                        continue;
                    }
                    (Fixup::ThreadLocal, _) | (_, Target::ThreadLocal(_)) => {
                        bail!("a thread-local reached another way")
                    }
                    (Fixup::Absolute, _) => {
                        let value = address(target, relocation.addend(), true)?;
                        store(&mut appended, place, value);
                        continue;
                    }
                    (Fixup::Relative, Target::Import(_)) => {
                        stub_of[&index].wrapping_add_signed(relocation.addend())
                    }
                    (Fixup::Relative, _) | (Fixup::ThroughSlot, Target::Import(_)) => {
                        let to = address(target, relocation.addend(), !code)?;
                        match stub_of.get(&index) {
                            Some(&stub)
                                if field == Field::Branch && !machine.reaches(to, place) =>
                            {
                                ensure!(relocation.addend() == 0, "a far branch into a function");
                                stub
                            }
                            _ => to,
                        }
                    }
                    (Fixup::ThroughSlot, _) => {
                        slot_of[&index].wrapping_add_signed(relocation.addend())
                    }
                };
                let bytes = appended[(place - base) as usize..].get_mut(..field.len());
                field.fill(bytes.context("a relocation past its section")?, to, place)?;
            }
            if section.name_bytes()? == b".eh_frame" {
                let relocated = &appended[start as usize..][..data.len()];
                frames.extend(Self::frames_in(relocated, base + start)?);
            }
            for (new, value) in in_place.iter().zip(values) {
                overwritten.push((image.offset_of(new.old.address)?, value));
            }
        }
        for (&index, &slot) in &slot_of {
            store(&mut appended, slot, address(self.targets[&index], 0, true)?);
        }
        for (&index, &stub) in &stub_of {
            let code = match self.targets[&index] {
                Target::Import(slot) => machine.stub_through(slot, stub)?,
                Target::Theirs(address) => machine.far_jump(address, stub)?,
                Target::Ours(..) | Target::Section(_) | Target::ThreadLocal(_) => continue,
            };
            appended[(stub - base) as usize..][..stub_len].copy_from_slice(&code);
        }

        let mut redirected = 0;
        let mut hops = FxHashSet::default();
        for replaced in &self.replaced {
            let Some(old) = replaced.old.filter(|it| it.kind == SymbolKind::Text) else { continue };
            let new = base + placed[&replaced.section] + replaced.offset;
            let cannot =
                || format!("{} cannot be jumped from", String::from_utf8_lossy(replaced.name));
            let jump = match old.room >= machine.jump() as u64 {
                true => machine.redirect(new, old.address, old.room).with_context(cannot)?,
                false => {
                    let pad = image.hop(&old, &hops).with_context(cannot)?;
                    hops.insert(pad);
                    let on = machine.redirect(new, pad, machine.jump() as u64)?;
                    overwritten.push((image.offset_of(pad)?, on));
                    // `jmp pad`, of a distance of one byte.
                    vec![0xeb, pad.wrapping_sub(old.address + 2) as u8]
                }
            };
            overwritten.push((image.offset_of(old.address)?, jump));
            // Its symbol names the new code, for whoever reads a backtrace.
            if old.entry != Defined::NO_ENTRY {
                let named = [new.to_le_bytes(), replaced.size.to_le_bytes()].concat();
                let value = old.entry + offset_of!(elf::Sym64<Endianness>, st_value) as u64;
                overwritten.push((value, named));
            }
            redirected += 1;
        }

        let unwind = image.unwind.moved(base + unwind_at, frames)?;
        appended[unwind_at as usize..][..unwind.len()].copy_from_slice(&unwind);
        if let Some(relocations) = relocations {
            let table = &mut appended[relocations_at as usize..];
            let old = relocations.table.len();
            table[..old].copy_from_slice(&relocations.table);
            ensure!(
                self.in_place.is_empty() || !relocations.packed,
                "a static with another value, where addresses are fixed up from a packed table"
            );
            let statics = self.in_place.values().flatten().map(|it| it.old).collect();
            let spare = base + spare_at;
            let relative = image.machine.relative();
            fixed.extend(LoadRelocations::restore(
                &mut table[..old],
                statics,
                restored,
                spare,
                relative,
            )?);
            let added = object::bytes_of_slice(&fixed);
            table[old..][..added.len()].copy_from_slice(added);
            let len = (old + added.len()) as u64;
            overwritten
                .push((relocations.address_at, (base + relocations_at).to_le_bytes().to_vec()));
            overwritten.push((relocations.size_at, len.to_le_bytes().to_vec()));
        }

        // The program headers, with the ones that moved pointing here and the new segments.
        let mut segments = image.segments.clone();
        let point_to =
            |header: &mut elf::ProgramHeader64<Endianness>, at: u64, address: u64, len: u64| {
                header.p_offset.set(e, offset + at);
                header.p_vaddr.set(e, address);
                header.p_paddr.set(e, address);
                header.p_filesz.set(e, len);
                header.p_memsz.set(e, len);
            };
        let point = |header: &mut elf::ProgramHeader64<Endianness>, at: u64, len: u64| {
            point_to(header, at, base + at, len)
        };
        point(&mut segments[image.unwind.segment], unwind_at, unwind.len() as u64);
        if let Some(header) = segments.iter_mut().find(|it| it.p_type(e) == elf::PT_PHDR) {
            point_to(header, headers_at, headers_address, headers_len as u64);
        }
        let loaded = |flags: u32| elf::ProgramHeader64::<Endianness> {
            p_type: object::U32::new(e, elf::PT_LOAD),
            p_flags: object::U32::new(e, flags),
            p_offset: object::U64::new(e, 0),
            p_vaddr: object::U64::new(e, 0),
            p_paddr: object::U64::new(e, 0),
            p_filesz: object::U64::new(e, 0),
            p_memsz: object::U64::new(e, 0),
            p_align: object::U64::new(e, page),
        };
        for segment in Segment::ALL {
            let (start, end) = extents[segment as usize];
            let mut header = loaded(segment.flags());
            point(&mut header, start, end - start);
            segments.push(header);
        }
        let mut header = loaded(elf::PF_R);
        point_to(&mut header, headers_at, headers_address, headers_len as u64);
        segments.push(header);
        let headers = object::bytes_of_slice(&segments).to_vec();
        let headers_offset = offset + headers_at;
        // What a debugger reads is no reason to link in full: without it the patch's code has
        // no lines, and the program's are where they were.
        let described = self.describe(
            &image.described,
            copies,
            machine,
            (headers_offset + headers.len() as u64).max(image.tail()),
            |target, added| address(target, added, false),
        );
        let mut header = image.header;
        if let Some((at, count)) = described.headers {
            header.e_shoff.set(e, at);
            header.e_shnum.set(e, count);
        }
        header.e_phoff.set(e, offset + headers_at);
        header.e_phnum.set(e, u16::try_from(segments.len())?);
        overwritten.push((0, object::bytes_of(&header).to_vec()));

        let live_frames = self.sections[Segment::Read as usize]
            .iter()
            .find_map(|&index| {
                let section = self.file.section_by_index(index).ok()?;
                (section.name_bytes().ok()? == b".eh_frame")
                    .then(|| (placed[&index], section.size()))
            })
            .unwrap_or((0, 0));

        let defined = (self.replaced.iter())
            .map(|replaced| {
                let in_place = (self.in_place.get(&replaced.section)).is_some_and(|it| {
                    it.binary_search_by_key(&replaced.offset, |it| it.offset).is_ok()
                });
                let old = replaced.old.filter(|_| in_place);
                let defined = Defined {
                    address: old.map_or(base + placed[&replaced.section] + replaced.offset, |it| {
                        it.address
                    }),
                    size: replaced.size,
                    room: replaced.size,
                    kind: match code.contains(&replaced.section) {
                        true => SymbolKind::Text,
                        false => SymbolKind::Data,
                    },
                    entry: replaced.old.map_or(Defined::NO_ENTRY, |it| it.entry),
                };
                (Box::from(replaced.name), defined)
            })
            .collect();

        Ok(Addition {
            offset,
            appended,
            headers_offset,
            headers,
            overwritten,
            redirected,
            described,
            live_data_at: extents[Segment::Data as usize].0,
            live_frames,
            live_pointers: fixed.iter().map(|it| it.r_offset.get(e).saturating_sub(base)).collect(),
            defined,
        })
    }
}

/// What a splice writes.
pub struct Addition {
    /// Where in the file `appended` goes: behind what the linker wrote.
    offset: u64,
    appended: Vec<u8>,
    /// The program headers, which may be far behind the rest: nothing is written between.
    headers_offset: u64,
    headers: Vec<u8>,
    /// What changes of the file the linker wrote, by where it is.
    overwritten: Vec<(u64, Vec<u8>)>,
    redirected: usize,
    /// What a debugger reads of it, behind the headers.
    described: debug::Tail,
    live_data_at: u64,
    live_frames: (u64, u64),
    live_pointers: Vec<u64>,
    /// Where each symbol the patch defines is now, of an ELF executable.
    defined: Vec<(Box<[u8]>, Defined)>,
}

fn read_i32(bytes: &[u8]) -> i32 {
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The distance from `from` to `to`, where the code has four bytes for it.
fn relative(to: u64, from: u64) -> anyhow::Result<i32> {
    i32::try_from(to.wrapping_sub(from) as i64).context("the program is too large")
}

fn binary_id(build_id: &[u8]) -> [u8; 16] {
    let mut id = [0; 16];
    let len = build_id.len().min(id.len());
    id[..len].copy_from_slice(&build_id[..len]);
    id
}

#[cfg(test)]
mod tests {
    use super::{Field, Machine, Writing, has_baked};

    /// A patch has to define all the executable was linked with a patch of, but for a static
    /// it left behind, under ELF's names and Mach-O's; what said so before is not looked for.
    #[test]
    fn a_static_left_behind_is_not_as_it_first_was() {
        let names = |all: &[&str]| -> Vec<Box<[u8]>> {
            all.iter().map(|it| it.as_bytes().into()).collect()
        };
        let moved = format!("{}blaze::static::7", crate::MOVED_STATIC);
        let baked = names(&["f", "blaze::static::7"]);
        assert!(has_baked(&baked, [&b"f"[..], moved.as_bytes()]).is_ok());
        assert!(has_baked(&baked, [&b"f"[..]]).is_err());
        assert!(has_baked(&baked, [&b"blaze::static::7"[..]]).is_err());
        let baked = names(&["_f", "_blaze::static::7"]);
        assert!(has_baked(&baked, [&b"_f"[..], format!("_{moved}").as_bytes()]).is_ok());
        assert!(has_baked(&baked, [&b"_f"[..], moved.as_bytes()]).is_err());
        assert!(has_baked(&names(&["f", &moved]), [&b"f"[..]]).is_ok());
    }

    /// What was read of the names stays across a splice that writes a new file, as on macOS,
    /// and not across a link: the splice after a link starts from the file the link wrote.
    #[cfg(unix)]
    #[test]
    fn the_names_read_stay_across_a_splice_and_not_a_link() {
        use super::{KEPT_NAMES, file_key, kept_names, names_spliced};
        let dir = std::env::temp_dir().join(format!("blaze-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("program");
        // Each file is kept under another name, so that no inode is used again.
        let write = |nth: usize| {
            let new = dir.join(format!("program.{nth}"));
            std::fs::write(&new, format!("file {nth}")).unwrap();
            std::fs::hard_link(&new, dir.join(format!("kept.{nth}"))).unwrap();
            std::fs::rename(&new, &path).unwrap();
            file_key(&path).unwrap()
        };
        let linked = write(0);
        let names = std::sync::Arc::new(["f".to_owned()].into_iter().collect());
        super::lock(&KEPT_NAMES).insert(path.clone(), (linked, std::sync::Arc::clone(&names)));
        write(1);
        names_spliced(&path, Some(linked));
        assert!(std::sync::Arc::ptr_eq(&kept_names(&path).unwrap(), &names));
        // Linked again, and spliced: read again, and this file is no executable.
        let linked = write(2);
        write(3);
        names_spliced(&path, Some(linked));
        assert!(kept_names(&path).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A client starts the program the daemon writes in place once the daemon is through,
    /// not while the file is open for writing, which on Linux is "text file busy".
    #[cfg(unix)]
    #[test]
    fn a_program_written_in_place_starts_after_the_write() {
        // `true` from PATH (NixOS has no /usr/bin/true), under its own name: a coreutils of
        // one binary is the program its name says.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let found =
            std::env::split_paths(&path).map(|dir| dir.join("true")).find(|it| it.is_file());
        let dir = std::env::temp_dir().join(format!("blaze-writing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("true");
        std::fs::copy(found.expect("a `true` on PATH"), &program).unwrap();
        // A copy out of the Nix store is read-only, as the store is.
        let writable = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        std::fs::set_permissions(&program, writable).unwrap();
        let writing = Writing::open(&program).unwrap();
        let client = std::thread::spawn({
            let program = program.clone();
            move || {
                let started = super::unwritten(&program)
                    .and_then(|_unwritten| std::process::Command::new(&program).spawn());
                (std::time::Instant::now(), started.and_then(|mut it| it.wait()))
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        let written = std::time::Instant::now();
        drop(writing);
        let (started, ended) = client.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(started > written, "started while the file was open for writing");
        assert!(ended.unwrap().success());
    }

    /// What a splice let go of stays so in the copy written while the program runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_copy_keeps_no_blocks_of_what_was_let_go_of() {
        use super::{OpenOptions, copy_file, let_go};
        use std::os::unix::fs::MetadataExt as _;
        let dir = std::env::temp_dir().join(format!("blaze-holes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (path, copy) = (dir.join("program"), dir.join("copy"));
        let bytes: Vec<u8> = (0..4 << 20).map(|it: u32| (it % 251) as u8 + 1).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let_go(&file, 1 << 20, 2 << 20);
        file.sync_all().unwrap();
        drop(file);
        copy_file(&path, &copy).unwrap();
        let (read, held) = (std::fs::read(&copy).unwrap(), std::fs::metadata(&copy).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(read.len(), bytes.len());
        assert!(read[1 << 20..3 << 20].iter().all(|&it| it == 0));
        assert_eq!(read[..1 << 20], bytes[..1 << 20]);
        assert_eq!(read[3 << 20..], bytes[3 << 20..]);
        assert!(held.blocks() * 512 <= 2 << 20 | 1 << 16, "{} bytes held", held.blocks() * 512);
    }

    fn words(code: &[u8]) -> Vec<u32> {
        code.chunks(4).map(|it| u32::from_le_bytes(it.try_into().unwrap())).collect()
    }

    /// What `as` and `ld` write for the same instructions at the same addresses.
    #[test]
    fn aarch64_instructions() {
        let machine = Machine::Aarch64;
        let far = machine.far_jump(0x1235_5678, 0x10000).unwrap();
        assert_eq!(words(&far), [0xb009_1a30, 0x9119_e210, 0xd61f_0200]);
        let stub = machine.stub_through(0x7ff8, 0x1000c).unwrap();
        assert_eq!(words(&stub), [0xf0ff_ffb0, 0xf947_fe10, 0xd61f_0200]);
        assert_eq!(words(&machine.redirect(0x7ff_ff00, 0x10018, 4).unwrap()), [0x15ff_bfba]);
        let mut call = 0x9400_0000u32.to_le_bytes();
        Field::Branch.fill(&mut call, 0x400, 0x1001c).unwrap();
        assert_eq!(words(&call), [0x97ff_c0f9]);
        let mut load = 0x7940_0001u32.to_le_bytes();
        Field::InPage(1).fill(&mut load, 0x7ff8, 0x10024).unwrap();
        assert_eq!(words(&load), [0x795f_f001]);
    }

    /// A `b` reaches 128 MB: past that the jump is the long one, where it fits.
    #[test]
    fn aarch64_reach() {
        let machine = Machine::Aarch64;
        let (old, last) = (0x10000u64, 0x10000 + (128 << 20) - 4);
        assert!(machine.reaches(last, old) && !machine.reaches(last + 4, old));
        assert!(machine.reaches(0, 128 << 20) && !machine.reaches(0, (128 << 20) + 4));
        assert_eq!(machine.redirect(last, old, 4).unwrap().len(), 4);
        assert_eq!(machine.redirect(last + 4, old, 12).unwrap().len(), 12);
        assert!(machine.redirect(last + 4, old, 8).is_err());
        assert!(machine.redirect(old + (5 << 30), old, 64).is_err());
        assert!(Field::InPage(3).fill(&mut [0; 4], 0x7ffc, 0).is_err());
        // A distance of 8 bytes has room for what one of 4 turns down.
        let (to, place, mut distance) = (0x10000u64, 0x10000 + (5 << 30), [0; 8]);
        assert!(Field::Distance.fill(&mut distance, to, place).is_err());
        Field::FarDistance.fill(&mut distance, to, place).unwrap();
        assert_eq!(i64::from_le_bytes(distance), -(5 << 30));
        assert_eq!(Field::FarDistance.len(), 8);
    }
}
