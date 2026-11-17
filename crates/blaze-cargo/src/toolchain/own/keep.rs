//! What the download of one pinned file of blaze's toolchain keeps of the bytes it is given: a
//! sans-IO core. It decides, from the pin and the offsets alone, how much of a file an earlier
//! download left is gone on from ([`Keep::resume`]), which bytes of a piece the server sent
//! are new ([`Keep::piece`]: a server that ignores the range sends the whole from byte 0, of
//! which what is had is left out), when a file is longer than published, and at the end
//! whether it is whole and the file the Rust project published ([`Keep::end`]). It opens no
//! file, asks no server and waits on nothing: the driver (`own::download`) reads the part file,
//! asks the source, writes what this says is new and hands it to the unpacking.

#![deny(blaze::sans_io_core)]

use std::fmt;

/// One file's download: its pin, how much of it is had, and the hash of what is had.
pub(crate) struct Keep {
    pinned: blaze_fetch::Digest,
    /// The file's published length.
    bytes: u64,
    /// How much of it is had, from byte 0.
    have: u64,
    hashing: blaze_fetch::Hashing,
}

/// Why a download's bytes are not the file: each said with the file by the driver.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The pin's sha256 is not a digest.
    Pin(String),
    /// A piece reaches past the published length.
    TooLong { bytes: u64 },
    /// A piece begins past what is had: bytes between are missing.
    Gap { offset: u64, have: u64 },
    /// The download ended short of the published length.
    Short { have: u64, bytes: u64 },
    /// Whole, and not the file published: its sha256 differs.
    NotPublished,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Pin(why) => write!(f, "its pin in pins.txt is not a digest: {why}"),
            Refused::TooLong { bytes } => write!(f, "it is longer than the {bytes} bytes published"),
            Refused::Gap { offset, have } => {
                write!(f, "a piece at byte {offset} came with only {have} bytes had before it")
            }
            Refused::Short { have, bytes } => {
                write!(f, "it ended at {have} of its {bytes} bytes, and blaze asks once")
            }
            Refused::NotPublished => f.write_str("its sha256 is not the one published"),
        }
    }
}

impl std::error::Error for Refused {}

impl Keep {
    /// The download of a file of `bytes` bytes whose sha256 is `sha256` (hex), nothing had yet.
    pub(crate) fn new(sha256: &str, bytes: u64) -> Result<Keep, Refused> {
        let text = format!("sha256:{sha256}");
        let pinned = blaze_fetch::Digest::parse(&text).map_err(|it| Refused::Pin(it.to_string()))?;
        // The pin's algorithm and BLAKE3 in one pass, as blaze-fetch checks every download.
        let hashing = blaze_fetch::Hashing::new(pinned.algo());
        Ok(Keep { pinned, bytes, have: 0, hashing })
    }

    /// How many bytes of a part file of `on_disk` bytes an earlier download left are gone on
    /// from: all of them, or none when it is longer than the file (it is then cut to none).
    pub(crate) fn resume(&self, on_disk: u64) -> u64 {
        if on_disk > self.bytes { 0 } else { on_disk }
    }

    /// `bytes` of the part file, the next after what is had: hashed and had.
    pub(crate) fn kept(&mut self, bytes: &[u8]) -> Result<(), Refused> {
        self.take(bytes)
    }

    /// The bytes of a piece the server sent at `offset` in the whole file that are new: none
    /// when every one is had, the rest when it begins inside what is had. They are hashed and
    /// had; the driver writes them and hands them on.
    pub(crate) fn piece<'a>(&mut self, offset: u64, bytes: &'a [u8]) -> Result<&'a [u8], Refused> {
        if offset > self.have {
            return Err(Refused::Gap { offset, have: self.have });
        }
        let had = usize::try_from(self.have - offset).unwrap_or(usize::MAX).min(bytes.len());
        let new = &bytes[had..];
        self.take(new)?;
        Ok(new)
    }

    /// How much of the file is had: where the next ask begins.
    pub(crate) fn have(&self) -> u64 {
        self.have
    }

    /// Whether every published byte is had.
    pub(crate) fn whole(&self) -> bool {
        self.have == self.bytes
    }

    /// The download's verdict once the source has sent everything it will: whole and the file
    /// published, or why not. A file that is not the one published is the driver's to remove.
    pub(crate) fn end(self) -> Result<(), Refused> {
        if self.have != self.bytes {
            return Err(Refused::Short { have: self.have, bytes: self.bytes });
        }
        if self.hashing.finish().digest != self.pinned {
            return Err(Refused::NotPublished);
        }
        Ok(())
    }

    fn take(&mut self, bytes: &[u8]) -> Result<(), Refused> {
        let after = self.have.saturating_add(bytes.len() as u64);
        if after > self.bytes {
            return Err(Refused::TooLong { bytes: self.bytes });
        }
        self.hashing.update(bytes);
        self.have = after;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256(bytes: &[u8]) -> String {
        crate::content::hex(&blaze_sha256::digest(bytes))
    }

    /// The invariant: whatever offsets the pieces come at (a range kept to, a server that sends
    /// the whole from 0 again, a piece overlapping what is had), the bytes said new are the
    /// file's next bytes, each once, and the verdict is whole and published exactly when every
    /// byte came and they are the pinned file. The control: a gap, a byte past the end, a
    /// short end and a changed byte are each refused.
    #[test]
    fn a_download_keeps_each_byte_of_the_file_once_and_checks_it_whole() {
        let file: Vec<u8> = (0..1000u32).map(|it| (it * 7 % 251) as u8).collect();
        let pin = sha256(&file);
        let mut keep = Keep::new(&pin, file.len() as u64).unwrap();
        assert_eq!(keep.resume(2000), 0, "a part longer than the file is not gone on from");
        assert_eq!(keep.resume(300), 300);
        keep.kept(&file[..300]).unwrap();
        let mut got = file[..300].to_vec();
        // A server that ignores the range: the whole from 0, in two pieces.
        got.extend_from_slice(keep.piece(0, &file[..200]).unwrap());
        got.extend_from_slice(keep.piece(200, &file[200..600]).unwrap());
        // A piece overlapping what is had, then the rest.
        got.extend_from_slice(keep.piece(550, &file[550..900]).unwrap());
        got.extend_from_slice(keep.piece(900, &file[900..]).unwrap());
        assert_eq!(got, file, "each byte once, in order");
        assert!(keep.whole());
        assert_eq!(keep.end(), Ok(()));

        let mut gap = Keep::new(&pin, file.len() as u64).unwrap();
        assert_eq!(gap.piece(10, &file[10..20]), Err(Refused::Gap { offset: 10, have: 0 }));
        let mut long = Keep::new(&pin, file.len() as u64).unwrap();
        assert_eq!(long.piece(0, &[file.as_slice(), b"x"].concat()), Err(Refused::TooLong { bytes: 1000 }));
        let mut short = Keep::new(&pin, file.len() as u64).unwrap();
        short.piece(0, &file[..999]).unwrap();
        assert_eq!(short.end(), Err(Refused::Short { have: 999, bytes: 1000 }));
        let mut changed = file.clone(); // clone: the control's other file
        changed[500] ^= 1;
        let mut other = Keep::new(&pin, file.len() as u64).unwrap();
        other.piece(0, &changed).unwrap();
        assert_eq!(other.end(), Err(Refused::NotPublished));
    }
}
