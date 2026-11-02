//! Typed values out of the configuration, as cargo's `src/context/de.rs`: `Config::get::<T>`
//! deserializes `T` from the value at a key, looking each field up in the files and its
//! environment variable in turn, so that what wins and the errors are cargo's.

use std::vec;

use blaze_hash::HashSet;
use serde::de::{self, IntoDeserializer};

use super::error::ConfigError;
use super::key::{ArrayItemKeyPath, ConfigKey};
use super::value;
use super::{Config, ConfigValue as CV, Definition, Value};
use crate::files::Files;

pub(crate) struct Deserializer<'c, F: Files> {
    pub(crate) config: &'c Config<F>,
    /// The key being deserialized.
    pub(crate) key: ConfigKey,
    /// Whether a variable that starts with this key's can stand for a table under it: true for
    /// `CARGO_PROFILE_DEV_BUILD_OVERRIDE_*`, false for `CARGO_BUILD_TARGET`, which would
    /// otherwise take `CARGO_BUILD_TARGET_DIR` for one.
    pub(crate) env_prefix_ok: bool,
}

impl<F: Files> Clone for Deserializer<'_, F> {
    fn clone(&self) -> Self {
        Deserializer { config: self.config, key: self.key.clone(), env_prefix_ok: self.env_prefix_ok }
    }
}

macro_rules! deserialize_method {
    ($method:ident, $visit:ident, $getter:ident) => {
        fn $method<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            let v =
                self.config.$getter(&self.key)?.ok_or_else(|| ConfigError::missing(&self.key))?;
            let Value { val, definition } = v;
            let res: Result<V::Value, ConfigError> = visitor.$visit(val);
            res.map_err(|e| e.with_key_context(&self.key, Some(definition)))
        }
    };
}

impl<'de, F: Files> de::Deserializer<'de> for Deserializer<'_, F> {
    type Error = ConfigError;

    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if let Some(cv) = self.config.get_cv_with_env(&self.key)? {
            let (res, def): (Result<V::Value, ConfigError>, Definition) = match cv {
                CV::Integer(i, def) => (visitor.visit_i64(i), def),
                CV::String(s, def) => (visitor.visit_string(s), def),
                CV::List(_, def) => (visitor.visit_seq(ConfigSeqAccess::new(self.clone())?), def),
                CV::Table(_, def) => {
                    (visitor.visit_map(ConfigMapAccess::new_map(self.clone())?), def)
                }
                CV::Boolean(b, def) => (visitor.visit_bool(b), def),
            };
            return res.map_err(|e| e.with_key_context(&self.key, Some(def)));
        }
        if self.config.has_key(&self.key, self.env_prefix_ok)? {
            return visitor.visit_some(self);
        }
        Err(ConfigError::missing(&self.key))
    }

    deserialize_method!(deserialize_bool, visit_bool, get_bool_value);
    deserialize_method!(deserialize_i8, visit_i64, get_integer_value);
    deserialize_method!(deserialize_i16, visit_i64, get_integer_value);
    deserialize_method!(deserialize_i32, visit_i64, get_integer_value);
    deserialize_method!(deserialize_i64, visit_i64, get_integer_value);
    deserialize_method!(deserialize_u8, visit_i64, get_integer_value);
    deserialize_method!(deserialize_u16, visit_i64, get_integer_value);
    deserialize_method!(deserialize_u32, visit_i64, get_integer_value);
    deserialize_method!(deserialize_u64, visit_i64, get_integer_value);
    deserialize_method!(deserialize_string, visit_string, get_string_value);

    fn deserialize_option<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.config.has_key(&self.key, self.env_prefix_ok)? {
            visitor.visit_some(self)
        } else {
            visitor.visit_none()
        }
    }

    fn deserialize_struct<V: de::Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if name == value::NAME && fields == value::FIELDS {
            let source = ValueSource::with_deserializer(self)?;
            return visitor.visit_map(ValueDeserializer::new(source));
        }
        visitor.visit_map(ConfigMapAccess::new_struct(self, fields)?)
    }

    fn deserialize_map<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_map(ConfigMapAccess::new_map(self)?)
    }

    fn deserialize_seq<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_seq(ConfigSeqAccess::new(self)?)
    }

    fn deserialize_tuple<V: de::Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_seq(ConfigSeqAccess::new(self)?)
    }

    fn deserialize_tuple_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_seq(ConfigSeqAccess::new(self)?)
    }

    fn deserialize_newtype_struct<V: de::Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if name != "StringList" {
            return visitor.visit_newtype_struct(self);
        }
        let mut res = Vec::new();
        match self.config.get_cv(&self.key)? {
            Some(CV::List(val, _)) => res.extend(val),
            Some(CV::String(val, def)) => {
                res.extend(val.split_whitespace().map(|s| CV::String(s.to_owned(), def.clone())))
            }
            Some(val) => {
                return Err(self
                    .config
                    .expected("string or array of strings", &self.key, &val)
                    .into());
            }
            None => {}
        }
        self.config.get_env_list(&self.key, &mut res);
        let vals: Vec<String> = res
            .into_iter()
            .map(|val| match val {
                CV::String(s, _) => Ok(s),
                other => Err(ConfigError::expected(&self.key, "string", &other)),
            })
            .collect::<Result<_, _>>()?;
        visitor.visit_newtype_struct(vals.into_deserializer())
    }

    fn deserialize_enum<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let Value { val, definition } = self
            .config
            .get_string_value(&self.key)?
            .ok_or_else(|| ConfigError::missing(&self.key))?;
        visitor
            .visit_enum(val.into_deserializer())
            .map_err(|e: ConfigError| e.with_key_context(&self.key, Some(definition)))
    }

    serde::forward_to_deserialize_any! {
        f32 f64 char str bytes
        byte_buf unit unit_struct
        identifier ignored_any
    }
}

struct ConfigMapAccess<'c, F: Files> {
    de: Deserializer<'c, F>,
    fields: Vec<KeyKind>,
    field_index: usize,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum KeyKind {
    Normal(String),
    CaseSensitive(String),
}

impl KeyKind {
    fn as_str(&self) -> &str {
        match self {
            KeyKind::Normal(s) | KeyKind::CaseSensitive(s) => s,
        }
    }
}

impl<'c, F: Files> ConfigMapAccess<'c, F> {
    fn new_map(de: Deserializer<'c, F>) -> Result<ConfigMapAccess<'c, F>, ConfigError> {
        let mut fields = Vec::new();
        if let Some(v) = de.config.get_table(&de.key)? {
            let mut keys: Vec<String> = v.val.into_keys().collect();
            // cargo's order is its hash map's; a fixed one keeps blaze's runs the same.
            keys.sort();
            fields.extend(keys.into_iter().map(KeyKind::CaseSensitive));
        }
        Ok(ConfigMapAccess { de, fields, field_index: 0 })
    }

    fn new_struct(
        de: Deserializer<'c, F>,
        given_fields: &'static [&'static str],
    ) -> Result<ConfigMapAccess<'c, F>, ConfigError> {
        let table = de.config.get_table(&de.key)?;
        if let Some(v) = table.as_ref() {
            let mut unused: Vec<_> =
                v.val.iter().filter(|(k, _)| !given_fields.iter().any(|gk| gk == k)).collect();
            unused.sort_by(|a, b| a.0.cmp(b.0));
            for (unused_key, unused_value) in unused {
                de.config.warn(format!(
                    "unused config key `{}.{unused_key}` in `{}`",
                    de.key,
                    unused_value.definition()
                ));
            }
        }

        let mut fields = HashSet::default();
        for field in given_fields {
            let mut field_key = de.key.clone();
            field_key.push(field);
            for env_key in de.config.env_keys() {
                let Some(nested_field) = env_key.strip_prefix(field_key.as_env_key()) else {
                    continue;
                };
                // `CARGO_UNSTABLE_GITOXIDE_FETCH` is no field of `unstable.git`.
                if nested_field.is_empty() || nested_field.starts_with('_') {
                    fields.insert(KeyKind::Normal((*field).to_owned()));
                }
            }
        }
        if let Some(v) = table {
            for key in v.val.keys() {
                fields.insert(KeyKind::Normal(key.clone()));
            }
        }
        let mut fields: Vec<_> = fields.into_iter().collect();
        fields.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ConfigMapAccess { de, fields, field_index: 0 })
    }
}

impl<'de, F: Files> de::MapAccess<'de> for ConfigMapAccess<'_, F> {
    type Error = ConfigError;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        if self.field_index >= self.fields.len() {
            return Ok(None);
        }
        let field = self.fields[self.field_index].as_str();
        seed.deserialize(field.into_deserializer()).map(Some)
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let field = &self.fields[self.field_index];
        self.field_index += 1;
        let field = match field {
            KeyKind::Normal(field) => {
                self.de.key.push(field);
                field
            }
            KeyKind::CaseSensitive(field) => {
                self.de.key.push_sensitive(field);
                field
            }
        };
        // A field whose variable is a prefix of another field's (`CARGO_BUILD_TARGET` of
        // `CARGO_BUILD_TARGET_DIR`) cannot be a table filled from variables.
        let env_prefix = format!("{field}_").replace('-', "_");
        let env_prefix_ok = !self
            .fields
            .iter()
            .any(|field| field.as_str().replace('-', "_").starts_with(&env_prefix));

        let result = seed
            .deserialize(Deserializer {
                config: self.de.config,
                key: self.de.key.clone(),
                env_prefix_ok,
            })
            .map_err(|e| {
                if !e.is_missing_field() {
                    return e;
                }
                let definition = self
                    .de
                    .config
                    .get_cv_with_env(&self.de.key)
                    .ok()
                    .and_then(|cv| cv.map(|cv| cv.definition().clone()));
                e.with_key_context(&self.de.key, definition)
            });
        self.de.key.pop();
        result
    }
}

struct ConfigSeqAccess {
    list_iter: std::iter::Enumerate<vec::IntoIter<CV>>,
    key: ConfigKey,
}

impl ConfigSeqAccess {
    fn new<F: Files>(de: Deserializer<'_, F>) -> Result<ConfigSeqAccess, ConfigError> {
        let mut res = Vec::new();
        match de.config.get_cv(&de.key)? {
            Some(CV::List(val, _)) => res.extend(val),
            Some(val) => return Err(de.config.expected("list", &de.key, &val).into()),
            None => {}
        }
        de.config.get_env_list(&de.key, &mut res);
        Ok(ConfigSeqAccess { list_iter: res.into_iter().enumerate(), key: de.key })
    }
}

impl<'de> de::SeqAccess<'de> for ConfigSeqAccess {
    type Error = ConfigError;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        let Some((i, cv)) = self.list_iter.next() else { return Ok(None) };
        let mut key_path = ArrayItemKeyPath::new(self.key.clone());
        let definition = Some(cv.definition().clone());
        seed.deserialize(ArrayItemDeserializer { cv, key_path: &mut key_path })
            .map_err(|e| {
                key_path.push_index(i);
                e.with_array_item_key_context(&key_path, definition)
            })
            .map(Some)
    }
}

enum ValueSource<'c, 'err, F: Files> {
    /// A value addressed by its key.
    Deserializer { de: Deserializer<'c, F>, definition: Definition },
    /// A value inside an array, which no key addresses.
    ConfigValue { cv: CV, key_path: &'err mut ArrayItemKeyPath },
}

impl<'c, F: Files> ValueSource<'c, '_, F> {
    fn with_deserializer(de: Deserializer<'c, F>) -> Result<Self, ConfigError> {
        let env = de.key.as_env_key();
        let env_def = Definition::Environment(env.to_owned());
        let definition = match (de.config.env(env).is_some(), de.config.get_cv(&de.key)?) {
            (true, Some(cv)) => {
                if env_def.is_higher_priority(cv.definition()) {
                    env_def
                } else {
                    cv.definition().clone()
                }
            }
            (false, Some(cv)) => cv.definition().clone(),
            // A table made of `CARGO_FOO_BAR_*` variables with no `CARGO_FOO_BAR`.
            (_, None) => env_def,
        };
        Ok(Self::Deserializer { de, definition })
    }
}

struct ValueDeserializer<'c, 'err, F: Files> {
    hits: u32,
    source: ValueSource<'c, 'err, F>,
}

impl<'c, 'err, F: Files> ValueDeserializer<'c, 'err, F> {
    fn new(source: ValueSource<'c, 'err, F>) -> Self {
        Self { hits: 0, source }
    }

    fn definition(&self) -> &Definition {
        match &self.source {
            ValueSource::Deserializer { definition, .. } => definition,
            ValueSource::ConfigValue { cv, .. } => cv.definition(),
        }
    }
}

impl<'de, F: Files> de::MapAccess<'de> for ValueDeserializer<'_, '_, F> {
    type Error = ConfigError;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        self.hits += 1;
        match self.hits {
            1 => seed.deserialize(value::VALUE_FIELD.into_deserializer()).map(Some),
            2 => seed.deserialize(value::DEFINITION_FIELD.into_deserializer()).map(Some),
            _ => Ok(None),
        }
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        if self.hits == 1 {
            return match &mut self.source {
                ValueSource::Deserializer { de, definition } => seed
                    .deserialize(de.clone())
                    .map_err(|e| e.with_key_context(&de.key, Some(definition.clone()))),
                ValueSource::ConfigValue { cv, key_path } => {
                    seed.deserialize(ArrayItemDeserializer { cv: cv.clone(), key_path })
                }
            };
        }
        match self.definition() {
            Definition::BuiltIn => seed.deserialize(0.into_deserializer()),
            Definition::Path(path) => {
                seed.deserialize(Tuple2Deserializer(1i32, path.to_string_lossy()))
            }
            Definition::Environment(env) => {
                seed.deserialize(Tuple2Deserializer(2i32, env.as_str()))
            }
            Definition::Cli(path) => {
                let s = path.as_ref().map(|p| p.to_string_lossy()).unwrap_or_default();
                seed.deserialize(Tuple2Deserializer(3i32, s))
            }
        }
    }
}

/// An item of an array, of whatever type the visitor asks for.
struct ArrayItemDeserializer<'err> {
    cv: CV,
    key_path: &'err mut ArrayItemKeyPath,
}

impl<'de> de::Deserializer<'de> for ArrayItemDeserializer<'_> {
    type Error = ConfigError;

    fn deserialize_struct<V: de::Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if name == value::NAME && fields == value::FIELDS {
            // `F` is the files of the other variant's deserializer; an array item has none.
            let source = ValueSource::<'_, '_, crate::files::Disk>::ConfigValue {
                cv: self.cv,
                key_path: self.key_path,
            };
            return visitor.visit_map(ValueDeserializer::new(source));
        }
        visitor.visit_map(ArrayItemMapAccess::with_struct(self.cv, fields, self.key_path))
    }

    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.cv {
            CV::String(s, _) => visitor.visit_string(s),
            CV::Integer(i, _) => visitor.visit_i64(i),
            CV::Boolean(b, _) => visitor.visit_bool(b),
            l @ CV::List(..) => visitor.visit_seq(ArrayItemSeqAccess::new(l, self.key_path)),
            t @ CV::Table(..) => visitor.visit_map(ArrayItemMapAccess::new(t, self.key_path)),
        }
    }

    serde::forward_to_deserialize_any! {
        bool u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 char str string seq
        bytes byte_buf map option unit newtype_struct
        ignored_any unit_struct tuple_struct tuple enum identifier
    }
}

struct ArrayItemSeqAccess<'err> {
    items: std::iter::Enumerate<vec::IntoIter<CV>>,
    key_path: &'err mut ArrayItemKeyPath,
}

impl<'err> ArrayItemSeqAccess<'err> {
    fn new(cv: CV, key_path: &'err mut ArrayItemKeyPath) -> Self {
        let CV::List(list, _) = cv else { unreachable!("must be a list") };
        Self { items: list.into_iter().enumerate(), key_path }
    }
}

impl<'de> de::SeqAccess<'de> for ArrayItemSeqAccess<'_> {
    type Error = ConfigError;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.items.next() {
            Some((i, cv)) => seed
                .deserialize(ArrayItemDeserializer { cv, key_path: self.key_path })
                .inspect_err(|_| self.key_path.push_index(i))
                .map(Some),
            None => Ok(None),
        }
    }
}

struct ArrayItemMapAccess<'err> {
    cv: CV,
    keys: vec::IntoIter<String>,
    current_key: Option<String>,
    key_path: &'err mut ArrayItemKeyPath,
}

impl<'err> ArrayItemMapAccess<'err> {
    fn new(cv: CV, key_path: &'err mut ArrayItemKeyPath) -> Self {
        let CV::Table(map, _) = &cv else { unreachable!("must be a map") };
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        Self { cv, keys: keys.into_iter(), current_key: None, key_path }
    }

    fn with_struct(cv: CV, given_fields: &[&str], key_path: &'err mut ArrayItemKeyPath) -> Self {
        let keys: Vec<String> = given_fields.iter().map(|s| (*s).to_owned()).collect();
        Self { cv, keys: keys.into_iter(), current_key: None, key_path }
    }
}

impl<'de> de::MapAccess<'de> for ArrayItemMapAccess<'_> {
    type Error = ConfigError;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self.keys.next() {
            Some(key) => {
                self.current_key = Some(key.clone());
                seed.deserialize(key.into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let key = self.current_key.take().unwrap();
        match &self.cv {
            CV::Table(map, _) => match map.get(&key) {
                Some(cv) => seed
                    .deserialize(ArrayItemDeserializer { cv: cv.clone(), key_path: self.key_path })
                    .inspect_err(|_| self.key_path.push_key(key)),
                None => Err(ConfigError::new(
                    format!("missing config key `{key}`"),
                    self.cv.definition().clone(),
                )),
            },
            _ => Err(ConfigError::new("expected table".to_owned(), self.cv.definition().clone())),
        }
    }
}

/// Two values deserialized as a tuple, as serde's `StrDeserializer` is one.
struct Tuple2Deserializer<T, U>(T, U);

impl<'de, T, U> de::Deserializer<'de> for Tuple2Deserializer<T, U>
where
    T: IntoDeserializer<'de, ConfigError>,
    U: IntoDeserializer<'de, ConfigError>,
{
    type Error = ConfigError;

    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, ConfigError> {
        struct SeqVisitor<T, U> {
            first: Option<T>,
            second: Option<U>,
        }

        impl<'de, T, U> de::SeqAccess<'de> for SeqVisitor<T, U>
        where
            T: IntoDeserializer<'de, ConfigError>,
            U: IntoDeserializer<'de, ConfigError>,
        {
            type Error = ConfigError;

            fn next_element_seed<K: de::DeserializeSeed<'de>>(
                &mut self,
                seed: K,
            ) -> Result<Option<K::Value>, Self::Error> {
                if let Some(first) = self.first.take() {
                    return seed.deserialize(first.into_deserializer()).map(Some);
                }
                if let Some(second) = self.second.take() {
                    return seed.deserialize(second.into_deserializer()).map(Some);
                }
                Ok(None)
            }
        }

        visitor.visit_seq(SeqVisitor { first: Some(self.0), second: Some(self.1) })
    }

    serde::forward_to_deserialize_any! {
        bool u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 char str string seq
        bytes byte_buf map struct option unit newtype_struct
        ignored_any unit_struct tuple_struct tuple enum identifier
    }
}
