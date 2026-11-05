// Follows `src/tools/rustfmt/src/config/style_edition.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use crate::config::StyleEdition;

/// Defines the default value for the given style edition
#[allow(dead_code)]
pub trait StyleEditionDefault {
    type ConfigType;
    fn style_edition_default(style_edition: StyleEdition) -> Self::ConfigType;
}

/// macro to help implement `StyleEditionDefault` for config options
#[macro_export]
macro_rules! style_edition_default {
    ($ty:ident, $config_ty:ty, _ => $default:expr) => {
        impl $crate::config::style_edition::StyleEditionDefault for $ty {
            type ConfigType = $config_ty;

            fn style_edition_default(_: $crate::config::StyleEdition) -> Self::ConfigType {
                $default
            }
        }
    };
    ($ty:ident, $config_ty:ty, Edition2024 => $default_2024:expr, _ => $default_2015:expr) => {
        impl $crate::config::style_edition::StyleEditionDefault for $ty {
            type ConfigType = $config_ty;

            fn style_edition_default(
                style_edition: $crate::config::StyleEdition,
            ) -> Self::ConfigType {
                match style_edition {
                    $crate::config::StyleEdition::Edition2015
                    | $crate::config::StyleEdition::Edition2018
                    | $crate::config::StyleEdition::Edition2021 => $default_2015,
                    // TODO: should update macro when default_2027 becomes available
                    $crate::config::StyleEdition::Edition2024
                    | $crate::config::StyleEdition::Edition2027 => $default_2024,
                }
            }
        }
    };
}
