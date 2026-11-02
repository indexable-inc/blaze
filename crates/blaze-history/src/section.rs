//! The plane's history in the daemon's image (docs/v2/plan/design/restore.md, "The state is one
//! mapped image"): [`History::encode`]'s bytes in [`SectionId::PLANE_HISTORY`] under
//! [`SCHEMA`], so a restart predicts as the last run ended, with no file of its own beside the
//! image. This module is the one owner of the section's layout.
//!
//! The section is checked whole (one CRC32C a 64 KiB group) and decoded at restore rather than
//! read in place: its records are placed again when the configured table is of another size.
//! [`HistoryError::starts_fresh`] tells an image of another blaze (no section, another schema,
//! another version of the bytes) from one whose section is corrupt or does not decode.

use std::fmt;

use blaze_format::SchemaHash;
use blaze_image::{Builder, Corrupt, Image, Refused, SectionId, Stable};

use crate::{Config, DecodeError, History};

/// The section's schema: [`History::encode`]'s bytes, by their version.
pub const SCHEMA: SchemaHash =
    blaze_format::schema("blaze plane history 2: blaze_history::History::encode bytes, version blzhis01");

/// Writes `history` into `builder`'s image.
pub fn write(history: &History, builder: &mut Builder) {
    builder.add(SectionId::PLANE_HISTORY, SCHEMA, &history.encode());
}

/// The history `image` holds, in a table as `config` sizes it.
pub fn restore<S: Stable>(image: &Image<S>, config: Config) -> Result<History, HistoryError> {
    let section = image.section(SectionId::PLANE_HISTORY, SCHEMA)?;
    let bytes = image.whole(&section)?;
    History::decode(bytes, config).map_err(HistoryError::Decode)
}

/// Why the image's history was not restored.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HistoryError {
    /// No section, or one of another schema: an image of another blaze.
    Refused(Refused),
    Corrupt(Corrupt),
    /// The section's bytes are not a history; [`DecodeError::Version`] is one of another blaze.
    Decode(DecodeError),
}

impl HistoryError {
    /// Whether the image is of another blaze, whose history the daemon replaces with an empty
    /// one, rather than corrupt.
    pub fn starts_fresh(&self) -> bool {
        matches!(self, HistoryError::Refused(_) | HistoryError::Decode(DecodeError::Version))
    }
}

impl From<Refused> for HistoryError {
    fn from(refused: Refused) -> HistoryError {
        HistoryError::Refused(refused)
    }
}

impl From<Corrupt> for HistoryError {
    fn from(corrupt: Corrupt) -> HistoryError {
        HistoryError::Corrupt(corrupt)
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistoryError::Refused(refused) => refused.fmt(f),
            HistoryError::Corrupt(corrupt) => corrupt.fmt(f),
            HistoryError::Decode(error) => write!(f, "{}: {error}", SectionId::PLANE_HISTORY),
        }
    }
}

impl std::error::Error for HistoryError {}

#[cfg(test)]
mod tests {
    use blaze_graph::{Key, admission::TaskKind};

    use super::*;

    fn config() -> Config {
        Config { slots: 256, unseen_ns: 5_000, unseen_bytes: 1 << 20 }
    }

    fn learned() -> History {
        let mut history = History::new(config());
        for n in 0..5000u64 {
            let mut key = [0u8; 32];
            key[..8].copy_from_slice(&(n % 97).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_le_bytes());
            let key = Key(key);
            history.observe(TaskKind((n % 300) as u32), (n % 3 != 0).then_some(&key), n * 1_013, n * 97 % 100_000);
        }
        history
    }

    fn image_of(history: &History) -> Vec<u8> {
        let mut builder = Builder::new();
        write(history, &mut builder);
        builder.finish().unwrap()
    }

    fn image_holding(schema: SchemaHash, bytes: &[u8]) -> Image<Vec<u8>> {
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, schema, bytes);
        Image::open(builder.finish().unwrap()).unwrap()
    }

    #[test]
    fn the_history_survives_an_image() {
        let history = learned();
        let image = Image::open(image_of(&history)).unwrap();
        let restored = match restore(&image, config()) {
            Ok(it) => it,
            Err(error) => panic!("{error}"),
        };
        assert_eq!(restored.encode(), history.encode());
        for kind in (0..310).map(TaskKind) {
            assert_eq!(restored.predict(kind, None), history.predict(kind, None));
        }
    }

    #[test]
    fn a_corrupt_history_is_named_and_not_started_fresh() {
        let mut file = image_of(&learned());
        let section = Image::open(file.clone()).unwrap().section(SectionId::PLANE_HISTORY, SCHEMA).unwrap();
        file[section.at as usize + 100] ^= 4;
        let image = Image::open(file).unwrap();
        match restore(&image, config()) {
            Err(error @ HistoryError::Corrupt(corrupt)) => {
                assert_eq!(corrupt.section, SectionId::PLANE_HISTORY);
                assert!(!error.starts_fresh());
            }
            Err(other) => panic!("expected a corrupt history, got {other:?}"),
            Ok(_) => panic!("expected a corrupt history, got one restored"),
        }
    }

    #[test]
    fn an_image_of_another_blaze_starts_fresh() {
        let missing = Image::open(Builder::new().finish().unwrap()).unwrap();
        let other_schema = image_holding(blaze_format::schema("blaze plane history 1"), &learned().encode());
        let mut other_version = learned().encode();
        other_version[7] ^= 1;
        let other_version = image_holding(SCHEMA, &other_version);
        for (what, image) in [("missing", &missing), ("another schema", &other_schema), ("another version", &other_version)] {
            match restore(image, config()) {
                Err(error) => assert!(error.starts_fresh(), "{what}: {error}"),
                Ok(_) => panic!("{what}: restored"),
            }
        }
    }

    #[test]
    fn an_undecodable_history_is_an_error_naming_the_section() {
        let mut bytes = learned().encode();
        bytes.truncate(bytes.len() - 3);
        match restore(&image_holding(SCHEMA, &bytes), config()) {
            Err(error @ HistoryError::Decode(_)) => {
                assert!(!error.starts_fresh());
                assert!(error.to_string().starts_with(&SectionId::PLANE_HISTORY.to_string()), "{error}");
            }
            Err(other) => panic!("expected an undecodable history, got {other:?}"),
            Ok(_) => panic!("expected an undecodable history, got one restored"),
        }
    }
}
