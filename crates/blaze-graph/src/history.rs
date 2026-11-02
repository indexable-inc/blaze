//! The plane's history in the daemon's image (docs/v2/plan/design/restore.md, "The state is one
//! mapped image"): what its predictor learned ([`Predict::encode`]) in
//! [`SectionId::PLANE_HISTORY`], so a restart predicts as the last run ended, with no file of
//! its own beside the image.
//!
//! The section is [`Estimates::encode`]'s bytes: a few hundred kinds at 40 bytes, so it is
//! checked whole (one CRC32C a 64 KiB group) and decoded at restore, microseconds, rather than
//! read in place. A section that fails its CRC, is of another schema or does not decode is an
//! error naming it; the caller starts the history fresh, which changes how work is ordered
//! and admitted, never what is built.

#![forbid(unsafe_code)]

use std::fmt;

use blaze_format::SchemaHash;
use blaze_image::{Builder, Corrupt, Image, Refused, SectionId, Stable};

use crate::admission::{DecodeError, Estimates, Predict};

/// The section's schema: [`Estimates::encode`]'s format, by its version.
pub const SCHEMA: SchemaHash =
    blaze_format::schema("blaze plane history 1: Estimates::encode bytes, version blzest01");

/// Writes `predict`'s history into `builder`'s image.
pub fn write(predict: &impl Predict, builder: &mut Builder) {
    builder.add(SectionId::PLANE_HISTORY, SCHEMA, &predict.encode());
}

/// The history `image` holds.
pub fn restore<S: Stable>(image: &Image<S>) -> Result<Estimates, HistoryError> {
    let section = image.section(SectionId::PLANE_HISTORY, SCHEMA)?;
    let bytes = image.whole(&section)?;
    Estimates::decode(bytes).map_err(HistoryError::Decode)
}

/// Why the image's history was not restored.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HistoryError {
    Refused(Refused),
    Corrupt(Corrupt),
    Decode(DecodeError),
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
    use super::*;
    use crate::admission::TaskKind;

    fn learned() -> Estimates {
        let mut estimates = Estimates::new(1 << 20);
        for n in 0..5000u64 {
            estimates.observe(TaskKind((n % 300) as u32), n * 97 % 100_000);
        }
        estimates
    }

    fn image_of(estimates: &Estimates) -> Vec<u8> {
        let mut builder = Builder::new();
        write(estimates, &mut builder);
        builder.finish().unwrap()
    }

    #[test]
    fn the_history_survives_an_image() {
        let estimates = learned();
        let image = Image::open(image_of(&estimates)).unwrap();
        let restored = restore(&image).unwrap();
        assert_eq!(restored, estimates);
        for kind in (0..310).map(TaskKind) {
            assert_eq!(restored.predict(kind), estimates.predict(kind));
        }
    }

    #[test]
    fn a_corrupt_history_is_named() {
        let mut file = image_of(&learned());
        let section = Image::open(file.clone()).unwrap().section(SectionId::PLANE_HISTORY, SCHEMA).unwrap();
        file[section.at as usize + 100] ^= 4;
        let image = Image::open(file).unwrap();
        match restore(&image) {
            Err(HistoryError::Corrupt(corrupt)) => {
                assert_eq!(corrupt.section, SectionId::PLANE_HISTORY);
                assert!(corrupt.to_string().contains("the plane's history"), "{corrupt}");
            }
            other => panic!("expected a corrupt history, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_or_undecodable_history_is_refused() {
        let image = Image::open(Builder::new().finish().unwrap()).unwrap();
        assert!(matches!(restore(&image), Err(HistoryError::Refused(Refused::Missing { .. }))));
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, SCHEMA, b"blzest01 cut short");
        let image = Image::open(builder.finish().unwrap()).unwrap();
        let error = restore(&image).unwrap_err();
        assert!(matches!(error, HistoryError::Decode(_)));
        assert!(error.to_string().starts_with(&SectionId::PLANE_HISTORY.to_string()));
    }
}
