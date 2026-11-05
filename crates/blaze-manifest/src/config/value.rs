//! A value of the configuration with where it was defined, as cargo's `Value<T>`.
//!
//! `Value<T>` asks the deserializer for a struct of the reserved name and fields below; the
//! config deserializer (`de.rs`) answers with the value and then the definition, as a tuple of
//! the variant's number and its path or name. No other deserializer knows the protocol.

use std::cmp::Ordering;
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::de;

#[derive(Debug, PartialEq, Clone)]
pub struct Value<T> {
    pub val: T,
    pub definition: Definition,
}

pub type OptValue<T> = Option<Value<T>>;

pub(crate) const VALUE_FIELD: &str = "$__cargo_private_value";
pub(crate) const DEFINITION_FIELD: &str = "$__cargo_private_definition";
pub(crate) const NAME: &str = "$__cargo_private_Value";
pub(crate) static FIELDS: [&str; 2] = [VALUE_FIELD, DEFINITION_FIELD];

/// Where a value was defined, which decides its priority and what a relative path in it is
/// relative to. Two definitions of the same kind are equal.
#[derive(Clone, Debug, Eq)]
pub enum Definition {
    BuiltIn,
    /// A config file, by its path.
    Path(PathBuf),
    /// An environment variable, by its name.
    Environment(String),
    /// `--config`, with the file's path when the argument named a file.
    Cli(Option<PathBuf>),
}

impl Definition {
    pub(crate) fn rank(&self) -> u8 {
        match self {
            Definition::BuiltIn => 0,
            Definition::Path(_) => 1,
            Definition::Environment(_) => 2,
            Definition::Cli(_) => 3,
        }
    }

    pub fn is_higher_priority(&self, other: &Definition) -> bool {
        self.rank() > other.rank()
    }

    /// The directory a relative path defined here is relative to: the one above `.cargo/` for a
    /// file, the current directory otherwise.
    pub fn root<'a>(&'a self, cwd: &'a Path) -> &'a Path {
        match self {
            Definition::Path(p) | Definition::Cli(Some(p)) => p.parent().unwrap().parent().unwrap(),
            Definition::Environment(_) | Definition::Cli(None) | Definition::BuiltIn => cwd,
        }
    }
}

impl PartialEq for Definition {
    fn eq(&self, other: &Definition) -> bool {
        self.rank() == other.rank()
    }
}

impl PartialOrd for Definition {
    fn partial_cmp(&self, other: &Definition) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Definition {
    fn cmp(&self, other: &Definition) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl fmt::Display for Definition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Definition::Path(p) | Definition::Cli(Some(p)) => p.display().fmt(f),
            Definition::Environment(key) => write!(f, "environment variable `{key}`"),
            Definition::Cli(None) => write!(f, "--config cli option"),
            Definition::BuiltIn => write!(f, "default"),
        }
    }
}

impl<T> From<T> for Value<T> {
    fn from(val: T) -> Self {
        Self { val, definition: Definition::BuiltIn }
    }
}

impl<'de, T: de::Deserialize<'de>> de::Deserialize<'de> for Value<T> {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Value<T>, D::Error> {
        struct ValueVisitor<T>(PhantomData<T>);

        impl<'de, T: de::Deserialize<'de>> de::Visitor<'de> for ValueVisitor<T> {
            type Value = Value<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a value")
            }

            fn visit_map<V: de::MapAccess<'de>>(
                self,
                mut visitor: V,
            ) -> Result<Value<T>, V::Error> {
                if visitor.next_key::<FieldKey<true>>()?.is_none() {
                    return Err(de::Error::custom("value not found"));
                }
                let val: T = visitor.next_value()?;
                if visitor.next_key::<FieldKey<false>>()?.is_none() {
                    return Err(de::Error::custom("definition not found"));
                }
                let definition: Definition = visitor.next_value()?;
                Ok(Value { val, definition })
            }
        }

        deserializer.deserialize_struct(NAME, &FIELDS, ValueVisitor(PhantomData))
    }
}

/// The value field when `VALUE`, else the definition field.
struct FieldKey<const VALUE: bool>;

impl<'de, const VALUE: bool> de::Deserialize<'de> for FieldKey<VALUE> {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor(&'static str);

        impl de::Visitor<'_> for FieldVisitor {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a valid value field")
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<(), E> {
                if s == self.0 {
                    Ok(())
                } else {
                    Err(de::Error::custom("expected field with custom name"))
                }
            }
        }

        let expected = if VALUE { VALUE_FIELD } else { DEFINITION_FIELD };
        deserializer.deserialize_identifier(FieldVisitor(expected))?;
        Ok(FieldKey)
    }
}

impl<'de> de::Deserialize<'de> for Definition {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Definition, D::Error> {
        let (discr, value) = <(u32, String)>::deserialize(deserializer)?;
        match discr {
            0 => Ok(Definition::BuiltIn),
            1 => Ok(Definition::Path(value.into())),
            2 => Ok(Definition::Environment(value)),
            3 => Ok(Definition::Cli((!value.is_empty()).then(|| value.into()))),
            _ => Err(de::Error::custom(format!("unexpected definition {discr} {value}"))),
        }
    }
}
