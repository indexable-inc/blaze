//! What bytes are pinned by: a [`Digest`], one of [`Algo`]'s, written `<algo>:<hex>`
//! (`sha256:9f86…`), and the one streaming pass ([`Hashing`]) that computes the pinned digest
//! and the BLAKE3 the store files the bytes by.
//!
//! The names are the lower-case ones `sha256sum`, `sha512sum` and `b3sum` are named after, and
//! nothing else: SRI's base64 (`sha256-…`), Nix's base32 and upper-case names are refused, so a
//! digest has one spelling and a pin compares as text. md5 and sha1 are refused as weak by name,
//! and every other algorithm as unknown, naming the three that are taken. A git commit id is not
//! a digest: it selects a commit and never verifies one ([`crate::Source::Git`]).

use std::fmt;

/// An algorithm a pin may be in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Algo {
    Sha256,
    Sha512,
    /// Plain BLAKE3, as `b3sum` prints it: no key, no derive-key context.
    Blake3,
}

impl Algo {
    pub const ALL: [Algo; 3] = [Algo::Sha256, Algo::Sha512, Algo::Blake3];

    /// The name before the `:`.
    pub const fn name(self) -> &'static str {
        match self {
            Algo::Sha256 => "sha256",
            Algo::Sha512 => "sha512",
            Algo::Blake3 => "blake3",
        }
    }

    /// How many bytes a digest of it is.
    pub const fn len(self) -> usize {
        match self {
            Algo::Sha256 | Algo::Blake3 => 32,
            Algo::Sha512 => 64,
        }
    }

    /// Its byte in a key ([`crate::store`]); never reused.
    pub(crate) const fn code(self) -> u8 {
        match self {
            Algo::Sha256 => 1,
            Algo::Sha512 => 2,
            Algo::Blake3 => 3,
        }
    }

    fn named(name: &str) -> Option<Algo> {
        Algo::ALL.into_iter().find(|it| it.name() == name)
    }
}

/// The digest of some bytes in one [`Algo`]: what a fetch is pinned by and checked against.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    algo: Algo,
    /// The first `algo.len()` bytes are the digest; the rest are zero.
    bytes: [u8; 64],
}

/// Why a text is not a [`Digest`]: each names the text and what it lacks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DigestRefused {
    /// No `<algo>:` before the hex.
    NoAlgo { text: String },
    /// md5 or sha1: collisions can be made for both, so neither pins anything.
    Weak { algo: String, text: String },
    /// An algorithm blaze does not take.
    Unknown { algo: String, text: String },
    /// The part after `:` is not hex.
    NotHex { algo: Algo, text: String },
    /// The hex is not as long as a digest of `algo`.
    Length { algo: Algo, text: String, digits: usize },
}

impl fmt::Display for DigestRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let taken = "sha256, sha512 or blake3";
        match self {
            DigestRefused::NoAlgo { text } => {
                write!(f, "`{text}` is no digest: a digest is `<algo>:<hex>`, the algo {taken}")
            }
            DigestRefused::Weak { algo, text } => write!(
                f,
                "`{text}` is a digest in {algo}, which is refused as weak (collisions can be made \
                 for it, so it pins nothing): pin the bytes by {taken}"
            ),
            DigestRefused::Unknown { algo, text } => write!(
                f,
                "`{text}` is a digest in `{algo}`, which blaze does not take: the algo is {taken}, \
                 in lower case"
            ),
            DigestRefused::NotHex { algo, text } => {
                write!(f, "`{text}`: the part after `{}:` is not hex", algo.name())
            }
            DigestRefused::Length { algo, text, digits } => write!(
                f,
                "`{text}` has {digits} hex digits, and a {} digest has {}",
                algo.name(),
                algo.len() * 2
            ),
        }
    }
}

impl std::error::Error for DigestRefused {}

impl Digest {
    /// The digest `bytes` in `algo`; `None` when `bytes` is not as long as one.
    pub fn new(algo: Algo, bytes: &[u8]) -> Option<Digest> {
        if bytes.len() != algo.len() {
            return None;
        }
        let mut all = [0; 64];
        all[..bytes.len()].copy_from_slice(bytes);
        Some(Digest { algo, bytes: all })
    }

    /// `<algo>:<hex>`, as [`Digest`]'s `Display` writes it; hex digits of either case.
    pub fn parse(text: &str) -> Result<Digest, DigestRefused> {
        let Some((name, hex)) = text.split_once(':') else {
            return Err(DigestRefused::NoAlgo { text: text.to_owned() }); // alloc: error path
        };
        let Some(algo) = Algo::named(name) else {
            let lower = name.to_ascii_lowercase();
            let weak = matches!(lower.as_str(), "md5" | "sha1" | "sha-1");
            return Err(match weak {
                true => DigestRefused::Weak { algo: name.to_owned(), text: text.to_owned() }, // alloc: error path
                false => DigestRefused::Unknown { algo: name.to_owned(), text: text.to_owned() }, // alloc: error path
            });
        };
        if !hex.bytes().all(|it| it.is_ascii_hexdigit()) {
            return Err(DigestRefused::NotHex { algo, text: text.to_owned() }); // alloc: error path
        }
        let refused = || DigestRefused::Length { algo, text: text.to_owned(), digits: hex.len() }; // alloc: error path
        let bytes: Option<Vec<u8>> = match algo.len() {
            32 => blaze_format::hex::decode::<32>(hex).map(|it| it.to_vec()), // alloc: once per pin read
            64 => blaze_format::hex::decode::<64>(hex).map(|it| it.to_vec()), // alloc: once per pin read
            _ => None,
        };
        bytes.and_then(|it| Digest::new(algo, &it)).ok_or_else(refused)
    }

    /// The digest of `bytes` in `algo`, in one pass.
    pub fn of(algo: Algo, bytes: &[u8]) -> Digest {
        let mut hashing = Hashing::new(algo);
        hashing.update(bytes);
        hashing.finish().digest
    }

    pub fn algo(&self) -> Algo {
        self.algo
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.algo.len()]
    }

    /// The hex digits, lower case, without the algo.
    pub fn hex(&self) -> String {
        blaze_format::hex::encode(self.bytes())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo.name(), self.hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::str::FromStr for Digest {
    type Err = DigestRefused;

    fn from_str(text: &str) -> Result<Digest, DigestRefused> {
        Digest::parse(text)
    }
}

impl serde::Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Digest, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Digest::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Plain BLAKE3 of some bytes, as `b3sum` prints it: what the store files an object by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Blake3(pub [u8; 32]);

impl Blake3 {
    pub fn of(bytes: &[u8]) -> Blake3 {
        Blake3(blaze_ids::b3::hash(bytes))
    }

    /// As a store key's input. A plain BLAKE3, not a derive-key one: only a key built in this
    /// crate's own schema reads it ([`crate::store`]).
    pub fn id(&self) -> blaze_ids::ContentId {
        blaze_ids::ContentId(self.0)
    }

    pub fn hex(&self) -> String {
        blaze_format::hex::encode(&self.0)
    }
}

impl fmt::Display for Blake3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blake3:{}", self.hex())
    }
}

/// The pinned algorithm's hasher and BLAKE3's, fed together: bytes are read once, whatever
/// the pin is in. A blake3 pin is one hasher, the store's.
pub struct Hashing {
    other: Other,
    blake3: blaze_ids::b3::Hasher,
    len: u64,
}

enum Other {
    None,
    Sha256(blaze_sha256::Sha256),
    Sha512(sha2::Sha512),
}

/// What a [`Hashing`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hashed {
    /// In the pin's algorithm.
    pub digest: Digest,
    pub blake3: Blake3,
    pub len: u64,
}

impl Hashed {
    /// `bytes` hashed in `algo` and BLAKE3, in one pass.
    pub fn of(algo: Algo, bytes: &[u8]) -> Hashed {
        let mut hashing = Hashing::new(algo);
        hashing.update(bytes);
        hashing.finish()
    }
}

impl Hashing {
    pub fn new(algo: Algo) -> Hashing {
        let other = match algo {
            Algo::Sha256 => Other::Sha256(blaze_sha256::Sha256::new()),
            Algo::Sha512 => Other::Sha512(<sha2::Sha512 as sha2::Digest>::new()),
            Algo::Blake3 => Other::None,
        };
        Hashing { other, blake3: blaze_ids::b3::Hasher::new(), len: 0 }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.blake3.update(bytes);
        match &mut self.other {
            Other::None => {}
            Other::Sha256(it) => {
                it.update(bytes);
            }
            Other::Sha512(it) => sha2::Digest::update(it, bytes),
        }
        self.len += bytes.len() as u64;
    }

    pub fn finish(self) -> Hashed {
        let blake3 = Blake3(self.blake3.finalize());
        let digest = match self.other {
            Other::None => Digest::new(Algo::Blake3, &blake3.0),
            Other::Sha256(it) => Digest::new(Algo::Sha256, &it.finish()),
            Other::Sha512(it) => Digest::new(Algo::Sha512, &sha2::Digest::finalize(it)),
        };
        Hashed {
            digest: digest.expect("each hasher's output is its algorithm's length"),
            blake3,
            len: self.len,
        }
    }
}
