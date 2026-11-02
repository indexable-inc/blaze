//! The inputs kept, how one is picked to change, and their files: hexadecimal, whitespace
//! ignored, as `blaze_fuzz::inputs` reads a corpus directory.
//!
//! Memory by lifetime (docs/v2/plan/design/memory.md): the kept inputs live as long as the
//! process, so they are columns over reserved regions (`blaze_mem::RegionVec`), which grow
//! by committing pages and never move. Keeping an input copies its bytes onto the end and
//! pushes two numbers, with no call into the general-purpose allocator. A kept input's file
//! is written from [`Scratch`] buffers that grow once and are reused.

use std::{
    ffi::{OsStr, OsString},
    path::{MAIN_SEPARATOR_STR, Path, PathBuf},
};

use blaze_mem::{MemError, RegionVec, Tag};

use crate::mutate::{self, Random};

/// The subsystem the fuzzer's memory is counted under.
const TAG: Tag = Tag::Other;
/// Address space for the kept inputs' bytes, reserved and committed as they grow.
const MOST_BYTES: usize = 1 << 34;
/// The most inputs a run can keep.
const MOST_INPUTS: usize = 1 << 26;

/// The inputs that reached a feature no input before them did, each weighted by how many it
/// was first to reach, the later ones more (libFuzzer's vanilla schedule favors the new).
pub(crate) struct Corpus {
    /// Every kept input's bytes, one after another.
    bytes: RegionVec<u8>,
    /// Where each input ends in `bytes`.
    ends: RegionVec<u64>,
    /// The running sum of the weights, for a pick by binary search.
    sums: RegionVec<u64>,
}

impl Corpus {
    /// Reserves the columns' address space; nothing is committed until an input is kept.
    pub(crate) fn new() -> Result<Corpus, MemError> {
        Ok(Corpus {
            bytes: RegionVec::new(MOST_BYTES, TAG)?,
            ends: RegionVec::new(MOST_INPUTS, TAG)?,
            sums: RegionVec::new(MOST_INPUTS, TAG)?,
        })
    }

    pub(crate) fn add(&mut self, input: &[u8], new_features: usize) -> Result<(), MemError> {
        let weight = new_features as u64 * (self.ends.len() as u64 + 1);
        let sum = self.sums.last().copied().unwrap_or(0) + weight.max(1);
        self.bytes.try_extend_from_slice(input)?;
        self.ends.try_push(self.bytes.len() as u64)?;
        self.sums.try_push(sum)?;
        Ok(())
    }

    fn input(&self, index: usize) -> &[u8] {
        let start = if index == 0 { 0 } else { self.ends[index - 1] as usize };
        &self.bytes[start..self.ends[index] as usize]
    }

    pub(crate) fn pick(&self, random: &mut Random) -> &[u8] {
        let total = self.sums.last().copied().unwrap_or(0);
        let at = random.below_u64(total);
        self.input(self.sums.partition_point(|&sum| sum <= at))
    }

    /// The input added last.
    #[cfg(test)]
    pub(crate) fn last(&self) -> &[u8] {
        if self.ends.is_empty() { &[] } else { self.input(self.ends.len() - 1) }
    }

    pub(crate) fn len(&self) -> usize {
        self.ends.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes.len()
    }
}

/// FNV-1a of an input: its file's name, the same on every machine.
pub(crate) fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// `hash` as 16 lowercase hexadecimal digits.
fn hex16(hash: u64) -> [u8; 16] {
    let mut digits = [0u8; 16];
    for (i, digit) in digits.iter_mut().enumerate() {
        *digit = HEX[(hash >> (60 - 4 * i)) as usize & 15];
    }
    digits
}

/// What names a file in a corpus directory among those already read: the content hash its
/// name spells (`<16 hex digits>.hex`, as [`write`] names it), or for a file named otherwise
/// the hash of its name.
pub(crate) fn key(name: &OsStr) -> u64 {
    let bytes = name.as_encoded_bytes();
    if let Some(digits) = bytes.strip_suffix(b".hex").filter(|it| it.len() == 16)
        && let Some(hash) = digits.iter().try_fold(0u64, |h, &d| Some(h << 4 | (d as char).to_digit(16)? as u64))
    {
        return hash;
    }
    hash(bytes)
}

/// Appends `bytes` in hexadecimal, 32 bytes to a line.
fn hex_into(bytes: &[u8], text: &mut String) {
    text.reserve(bytes.len() * 2 + bytes.len() / 32 + 1);
    for line in bytes.chunks(32) {
        for &byte in line {
            text.push(HEX[byte as usize >> 4] as char);
            text.push(HEX[byte as usize & 15] as char);
        }
        text.push('\n');
    }
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    let mut text = String::new();  // alloc: a whole file's text, at the edge
    hex_into(bytes, &mut text);
    text
}

pub(crate) fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = text.bytes().filter(|it| !it.is_ascii_whitespace()).collect();  // alloc: reading a corpus file, at load or sync
    if digits.len() % 2 == 1 {
        return Err(format!("{} hexadecimal digits: an odd count", digits.len()));  // alloc: error message
    }
    digits
        .chunks(2)
        .map(|pair| {
            let digit = |d: u8| (d as char).to_digit(16).ok_or_else(|| format!("{:?} is not a hexadecimal digit", d as char));  // alloc: error message
            Ok((digit(pair[0])? << 4 | digit(pair[1])?) as u8)
        })
        .collect()  // alloc: reading a corpus file, at load or sync
}

pub(crate) fn read(path: &Path) -> Result<Vec<u8>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;  // alloc: error message
    from_hex(&text).map_err(|e| format!("{}: {e}", path.display()))  // alloc: error message
}

/// The names of the files in `dir`, in name order (none when it doesn't exist), but those
/// starting with `.`, which are being written. One listing, no `stat` per file.
pub(crate) fn names(dir: &Path) -> Result<Vec<OsString>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),  // alloc: none (an empty Vec), a missing directory
        Err(e) => return Err(format!("{}: {e}", dir.display())),  // alloc: error message
    };
    let mut names = Vec::new();  // alloc: a directory listing, at load and once a second at most
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;  // alloc: error message
        let is_file = entry.file_type().map_err(|e| format!("{}: {e}", entry.path().display()))?.is_file();  // alloc: error message
        let name = entry.file_name();
        if is_file && !name.as_encoded_bytes().starts_with(b".") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// The inputs of every file in `dir`, in name order (none when it doesn't exist).
pub(crate) fn read_dir(dir: &Path) -> Result<Vec<Vec<u8>>, String> {
    names(dir)?.iter().map(|name| read(&dir.join(name))).collect()  // alloc: reading a corpus file, at load or sync
}

/// The buffers a file is written from: grown to the longest file and path once, then reused.
#[derive(Default)]
pub(crate) struct Scratch {
    text: String,
    path: PathBuf,
    partial: PathBuf,
}

impl Scratch {
    /// Writes `bytes` to `dir/<prefix><hash>.hex` and gives the path. The file appears
    /// whole: it is written under a `.` name and renamed, so a `--jobs` worker listing `dir`
    /// never reads a part.
    pub(crate) fn write(&mut self, dir: &Path, prefix: &str, bytes: &[u8]) -> Result<&Path, String> {
        let digits = hex16(hash(bytes));
        let digits = std::str::from_utf8(&digits).expect("hexadecimal digits are ASCII");
        let mut pid = [0u8; 20];
        let pid = std::str::from_utf8(mutate::decimal(std::process::id() as u64, &mut pid)).expect("decimal digits are ASCII");
        for (path, parts) in [(&mut self.path, ["", ""]), (&mut self.partial, [".", pid])] {
            let os = path.as_mut_os_string();
            os.clear();
            os.push(dir);
            os.push(MAIN_SEPARATOR_STR);
            os.push(parts[0]);
            os.push(parts[1]);
            if !parts[1].is_empty() {
                os.push("-");
            }
            os.push(prefix);
            os.push(digits);
            os.push(".hex");
        }
        self.text.clear();
        hex_into(bytes, &mut self.text);
        std::fs::write(&self.partial, &self.text).map_err(|e| format!("{}: {e}", self.partial.display()))?;  // alloc: error message
        std::fs::rename(&self.partial, &self.path)
            .map_err(|e| format!("{} to {}: {e}", self.partial.display(), self.path.display()))?;  // alloc: error message
        Ok(&self.path)
    }
}

/// [`Scratch::write`] once, for a file written at the edge (a crash, a minimized input).
pub(crate) fn write(dir: &Path, prefix: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    Scratch::default().write(dir, prefix, bytes).map(Path::to_path_buf)
}
