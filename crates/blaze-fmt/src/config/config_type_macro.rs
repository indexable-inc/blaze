//! `#[config_type]` on enums, as `rustfmt/config_proc_macro/src/item_enum.rs` at rust-lang/rust
//! 12433c307 (MIT or Apache-2.0) generates it, written as `macro_rules!` so no proc-macro crate is
//! needed. A variant takes `#[value = ".."]` (its config text), `#[doc_hint = ".."]` and
//! `#[unstable_variant]` among its doc comments; only unit variants are read from config text.

macro_rules! config_type {
    (
        $(#[$enum_attr:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$($vattr:tt)*])*
                $variant:ident $(($($field:ty),* $(,)?))?
            ),* $(,)?
        }
    ) => {
        $(#[$enum_attr])*
        #[derive(Debug, Copy, Clone, Eq, PartialEq)]
        $vis enum $name {
            $($variant $(($($field),*))?),*
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                $(
                    if matches!(self, $name::$variant { .. }) {
                        return f.write_str($crate::config::config_type_macro::config_type_value!(
                            $variant; $([$($vattr)*])*
                        ));
                    }
                )*
                unreachable!()
            }
        }

        impl $crate::config::ConfigType for $name {
            fn doc_hint() -> String {
                let hints: &[String] = &[$({
                    let mut text = String::from(
                        $crate::config::config_type_macro::config_type_doc_hint!(
                            $variant; $([$($vattr)*])*
                        ),
                    );
                    if $crate::config::config_type_macro::config_type_unstable!($([$($vattr)*])*) {
                        text.push_str(" (unstable)");
                    }
                    text
                }),*];
                format!("[{}]", hints.join("|"))
            }

            fn stable_variant(&self) -> bool {
                $(
                    if matches!(self, $name::$variant { .. }) {
                        return !$crate::config::config_type_macro::config_type_unstable!(
                            $([$($vattr)*])*
                        );
                    }
                )*
                unreachable!()
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = &'static str;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                $(
                    $crate::config::config_type_macro::config_type_if_unit!(
                        [$($($field)*)?]
                        if $crate::config::config_type_macro::config_type_value!(
                            $variant; $([$($vattr)*])*
                        )
                        .eq_ignore_ascii_case(s)
                        {
                            return Ok($name::$variant);
                        }
                    );
                )*
                let mut variants: Vec<&str> = Vec::new();
                $(
                    $crate::config::config_type_macro::config_type_if_unit!(
                        [$($($field)*)?]
                        variants.push(stringify!($variant));
                    );
                )*
                let message = format!(
                    "Bad variant, expected one of:{}",
                    variants.iter().map(|v| format!(" `{v}`")).collect::<String>()
                );
                Err(Box::leak(message.into_boxed_str()))
            }
        }

        impl ::serde::ser::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: ::serde::ser::Serializer,
            {
                $(
                    if matches!(self, $name::$variant { .. }) {
                        return serializer.serialize_str(
                            $crate::config::config_type_macro::config_type_value!(
                                $variant; $([$($vattr)*])*
                            ),
                        );
                    }
                )*
                unreachable!()
            }
        }

        impl<'de> ::serde::de::Deserialize<'de> for $name {
            fn deserialize<D>(d: D) -> Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                use ::serde::de::{Error, Visitor};
                use ::std::marker::PhantomData;
                struct StringOnly<T>(PhantomData<T>);
                impl<'de, T> Visitor<'de> for StringOnly<T>
                where
                    T: ::serde::Deserializer<'de>,
                {
                    type Value = String;
                    fn expecting(
                        &self,
                        formatter: &mut ::std::fmt::Formatter<'_>,
                    ) -> ::std::fmt::Result {
                        formatter.write_str("string")
                    }
                    fn visit_str<E>(self, value: &str) -> Result<String, E> {
                        Ok(String::from(value))
                    }
                }
                let s = &d.deserialize_string(StringOnly::<D>(PhantomData))?;
                let mut allowed: Vec<&'static str> = Vec::new();
                $(
                    $crate::config::config_type_macro::config_type_if_unit!(
                        [$($($field)*)?]
                        {
                            let value = $crate::config::config_type_macro::config_type_value!(
                                $variant; $([$($vattr)*])*
                            );
                            if value.eq_ignore_ascii_case(s) {
                                return Ok($name::$variant);
                            }
                            allowed.push(value);
                        }
                    );
                )*
                Err(D::Error::unknown_variant(s, Box::leak(allowed.into_boxed_slice())))
            }
        }
    };
}

/// Expands to the body only for a unit variant (no field types between the brackets).
macro_rules! config_type_if_unit {
    ([] $($body:tt)*) => { $($body)* };
    ([$($field:tt)+] $($body:tt)*) => {};
}

macro_rules! config_type_value {
    ($variant:ident;) => { stringify!($variant) };
    ($variant:ident; [value = $value:literal] $($rest:tt)*) => { $value };
    ($variant:ident; [$($other:tt)*] $($rest:tt)*) => {
        $crate::config::config_type_macro::config_type_value!($variant; $($rest)*)
    };
}

macro_rules! config_type_doc_hint {
    ($variant:ident;) => { stringify!($variant) };
    ($variant:ident; [doc_hint = $hint:literal] $($rest:tt)*) => { $hint };
    ($variant:ident; [$($other:tt)*] $($rest:tt)*) => {
        $crate::config::config_type_macro::config_type_doc_hint!($variant; $($rest)*)
    };
}

macro_rules! config_type_unstable {
    () => { false };
    ([unstable_variant] $($rest:tt)*) => { true };
    ([$($other:tt)*] $($rest:tt)*) => {
        $crate::config::config_type_macro::config_type_unstable!($($rest)*)
    };
}

pub(crate) use {
    config_type, config_type_doc_hint, config_type_if_unit, config_type_unstable, config_type_value,
};
