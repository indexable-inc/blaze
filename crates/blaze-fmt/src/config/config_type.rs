// Follows `src/tools/rustfmt/src/config/config_type.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use std::alloc::{Allocator, Global};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use blaze_data::{Int, Map, Value};
use blaze_toml::{Item, Kind};

use crate::config::file_lines::FileLines;
use crate::config::macro_names::{MacroSelector, MacroSelectors};
use crate::config::options::{IgnoreList, WidthHeuristics};

/// Trait for types that can be used in `Config`.
pub(crate) trait ConfigType: Sized {
    /// Returns hint text for use in `Config::print_docs()`. For enum types, this is a
    /// pipe-separated list of variants; for other types it returns `<type>`.
    fn doc_hint() -> String;

    /// Return `true` if the variant (i.e. value of this type) is stable.
    ///
    /// By default, return true for all values. Enums annotated with `#[config_type]`
    /// are automatically implemented, based on the `#[unstable_variant]` annotation.
    fn stable_variant(&self) -> bool {
        true
    }

    /// The option's value as a rustfmt.toml writes it, read from its TOML item; a value of
    /// another shape is refused, naming where in the item.
    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal>;

    /// The option's value as TOML, for `--print-config`.
    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm>;
}

/// Why a rustfmt.toml option's value was refused: the byte offset of the part refused, the
/// path from the option down to it (`.fn_call_width`, `[2]`; empty for the value itself), and
/// what is wrong with it.
#[derive(Debug)]
pub struct OptionRefusal {
    pub(crate) at: usize,
    pub(crate) below: String,
    pub(crate) what: Refusal,
}

impl OptionRefusal {
    fn new<A: Allocator>(item: Item<'_, '_, A>, what: Refusal) -> OptionRefusal {
        OptionRefusal {
            at: item.span().start,
            below: String::new(),
            what,
        }
    }

    fn expected<A: Allocator>(item: Item<'_, '_, A>, expected: &'static str) -> OptionRefusal {
        OptionRefusal::new(
            item,
            Refusal::Expected {
                expected,
                found: item.kind(),
            },
        )
    }

    /// The refusal of a part of the value, `step` (`.key` or `[index]`) below it.
    fn under(mut self, step: fmt::Arguments<'_>) -> OptionRefusal {
        self.below.insert_str(0, &step.to_string());
        self
    }
}

/// What is wrong with a rustfmt.toml option's value.
#[derive(Debug)]
pub enum Refusal {
    /// A value of another TOML type.
    Expected { expected: &'static str, found: Kind },
    /// A negative or too large integer where an unsigned one goes.
    NotUnsigned(i64),
    /// An integer past the platform's `isize`.
    NotSigned(i64),
    /// A string that names none of the option's values.
    UnknownVariant {
        found: String,
        allowed: Vec<&'static str>,
    },
    /// A table without a key it must have.
    MissingField(&'static str),
    /// A table with a key it does not take.
    UnknownField(String),
    /// `file_lines`, which only `--file-lines` sets.
    CliOnly,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Expected { expected, found } => {
                write!(f, "expected {expected}, found {}", found.name())
            }
            Refusal::NotUnsigned(n) => write!(f, "expected an unsigned integer, found {n}"),
            Refusal::NotSigned(n) => write!(f, "{n} is past a signed integer of this platform"),
            Refusal::UnknownVariant { found, allowed } => {
                write!(f, "unknown variant `{found}`, expected one of ")?;
                for (at, value) in allowed.iter().enumerate() {
                    if at > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "`{value}`")?;
                }
                Ok(())
            }
            Refusal::MissingField(field) => write!(f, "missing field `{field}`"),
            Refusal::UnknownField(field) => write!(f, "unknown field `{field}`"),
            Refusal::CliOnly => f.write_str(
                "FileLines cannot be deserialized from a project rustfmt.toml file: please \
                 specify it via the `--file-lines` option instead",
            ),
        }
    }
}

/// Why an option's value has no TOML form.
#[derive(Debug)]
pub enum NoTomlForm {
    /// A path that is not UTF-8.
    NotUtf8(PathBuf),
    /// `file_lines`, which no rustfmt.toml holds.
    CliOnly,
}

impl fmt::Display for NoTomlForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoTomlForm::NotUtf8(path) => {
                write!(f, "path {path:?} can't be serialized as UTF-8 string")
            }
            NoTomlForm::CliOnly => f.write_str("FileLines cannot be serialized"),
        }
    }
}

/// `text` as a TOML string value.
pub(crate) fn toml_text(text: &str) -> Value<Global> {
    Value::Text(blaze_own::String::from_str_in(text, Global))
}

/// The strings of a TOML array, each refused by its index when it is not one.
fn toml_strings<'d, A: Allocator>(
    item: Item<'d, '_, A>,
) -> Result<Vec<&'d str>, OptionRefusal> {
    if !item.is_array() {
        return Err(OptionRefusal::expected(item, "an array of strings"));
    }
    item.children()
        .enumerate()
        .map(|(at, element)| {
            element.as_str().ok_or_else(|| {
                OptionRefusal::expected(element, "a string").under(format_args!("[{at}]"))
            })
        })
        .collect() // alloc: the strings of one option's array, read once
}

/// `texts` as a TOML array of strings.
fn toml_list<'s>(texts: impl Iterator<Item = &'s str>) -> Value<Global> {
    let mut list = blaze_own::Vec::new_in(Global);
    for text in texts {
        list.push(toml_text(text));
    }
    Value::List(list)
}

impl ConfigType for bool {
    fn doc_hint() -> String {
        String::from("<boolean>")
    }

    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal> {
        item.as_bool()
            .ok_or_else(|| OptionRefusal::expected(item, "a boolean"))
    }

    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm> {
        Ok(Value::Bool(*self))
    }
}

impl ConfigType for usize {
    fn doc_hint() -> String {
        String::from("<unsigned integer>")
    }

    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal> {
        let n = item
            .as_integer()
            .ok_or_else(|| OptionRefusal::expected(item, "an unsigned integer"))?;
        usize::try_from(n).map_err(|_| OptionRefusal::new(item, Refusal::NotUnsigned(n)))
    }

    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm> {
        Ok(Value::Int(Int::from_u64(*self as u64)))
    }
}

impl ConfigType for isize {
    fn doc_hint() -> String {
        String::from("<signed integer>")
    }

    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal> {
        let n = item
            .as_integer()
            .ok_or_else(|| OptionRefusal::expected(item, "a signed integer"))?;
        isize::try_from(n).map_err(|_| OptionRefusal::new(item, Refusal::NotSigned(n)))
    }

    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm> {
        Ok(Value::Int(Int::from_i64(*self as i64)))
    }
}

impl ConfigType for String {
    fn doc_hint() -> String {
        String::from("<string>")
    }

    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal> {
        item.as_str()
            .map(str::to_owned)
            .ok_or_else(|| OptionRefusal::expected(item, "a string"))
    }

    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm> {
        Ok(toml_text(self))
    }
}

impl ConfigType for FileLines {
    fn doc_hint() -> String {
        String::from("<json>")
    }

    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal> {
        Err(OptionRefusal::new(item, Refusal::CliOnly))
    }

    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm> {
        Err(NoTomlForm::CliOnly)
    }
}

impl ConfigType for MacroSelectors {
    fn doc_hint() -> String {
        String::from("[<string>, ...]")
    }

    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal> {
        let selectors = toml_strings(item)?
            .into_iter()
            .map(|name| {
                let Ok(selector) = MacroSelector::from_str(name);
                selector
            })
            .collect(); // alloc: the option's selectors, kept in the config
        Ok(MacroSelectors(selectors))
    }

    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm> {
        let names: Vec<String> = self.0.iter().map(ToString::to_string).collect(); // alloc: the selectors as text, written once
        Ok(toml_list(names.iter().map(String::as_str)))
    }
}

impl ConfigType for WidthHeuristics {
    fn doc_hint() -> String {
        String::new()
    }

    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal> {
        if !item.is_table() {
            return Err(OptionRefusal::expected(item, "a table"));
        }
        let mut fields: [Option<usize>; 8] = [None; 8];
        for field in item.children() {
            let Some(at) = WIDTH_HEURISTICS.iter().position(|name| *name == field.key()) else {
                return Err(OptionRefusal::new(
                    field,
                    Refusal::UnknownField(field.key().to_owned()),
                ));
            };
            let width = usize::from_toml(field)
                .map_err(|refusal| refusal.under(format_args!(".{}", WIDTH_HEURISTICS[at])))?;
            fields[at] = Some(width);
        }
        let field = |at: usize| {
            fields[at].ok_or_else(|| {
                OptionRefusal::new(item, Refusal::MissingField(WIDTH_HEURISTICS[at]))
            })
        };
        Ok(WidthHeuristics {
            fn_call_width: field(0)?,
            attr_fn_like_width: field(1)?,
            struct_lit_width: field(2)?,
            struct_variant_width: field(3)?,
            array_width: field(4)?,
            chain_width: field(5)?,
            single_line_if_else_max_width: field(6)?,
            single_line_let_else_max_width: field(7)?,
        })
    }

    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm> {
        let widths = [
            self.fn_call_width,
            self.attr_fn_like_width,
            self.struct_lit_width,
            self.struct_variant_width,
            self.array_width,
            self.chain_width,
            self.single_line_if_else_max_width,
            self.single_line_let_else_max_width,
        ];
        let mut map = Map::new_in(Global);
        for (name, width) in WIDTH_HEURISTICS.iter().zip(widths) {
            map.insert(
                blaze_own::String::from_str_in(name, Global),
                width.to_toml()?,
            );
        }
        Ok(Value::Map(map))
    }
}

/// The keys of `width_heuristics`, in the order of `WidthHeuristics`'s fields.
const WIDTH_HEURISTICS: [&str; 8] = [
    "fn_call_width",
    "attr_fn_like_width",
    "struct_lit_width",
    "struct_variant_width",
    "array_width",
    "chain_width",
    "single_line_if_else_max_width",
    "single_line_let_else_max_width",
];

impl ConfigType for IgnoreList {
    fn doc_hint() -> String {
        String::from("[<string>,..]")
    }

    fn from_toml<A: Allocator>(item: Item<'_, '_, A>) -> Result<Self, OptionRefusal> {
        let paths: HashSet<PathBuf> = toml_strings(item)?
            .into_iter()
            .map(PathBuf::from)
            .collect(); // alloc: the option's paths, kept in the config
        Ok(IgnoreList::from_paths(paths))
    }

    fn to_toml(&self) -> Result<Value<Global>, NoTomlForm> {
        let paths: Vec<&str> = self
            .into_iter()
            .map(|path| {
                path.to_str()
                    .ok_or_else(|| NoTomlForm::NotUtf8(path.to_owned()))
            })
            .collect::<Result<_, _>>()?; // alloc: the paths as text, written once
        Ok(toml_list(paths.into_iter()))
    }
}

macro_rules! create_config {
    // Options passed into the macro.
    //
    // - $i: the ident name of the option
    // - $ty: the type of the option value
    // - $stb: true if the option is stable
    // - $dstring: description of the option
    ($($i:ident: $ty:ty, $stb:expr, $( $dstring:expr ),+ );+ $(;)*) => (
        use std::io::Write;

        use $crate::config::style_edition::StyleEditionDefault;

        #[derive(Clone)]
        #[allow(unreachable_pub)]
        pub struct Config {
            // For each config item, we store:
            //
            // - 0: true if the value has been access
            // - 1: true if the option was manually initialized
            // - 2: the option value
            // - 3: true if the option is unstable
            // - 4: true if the option was set manually from a CLI flag
            // FIXME: 4 is probably unnecessary and duplicative
            // https://github.com/rust-lang/rustfmt/issues/6252
            $($i: (Cell<bool>, bool, <$ty as StyleEditionDefault>::ConfigType, bool, bool)),+
        }

        // Just like the Config struct but with each property wrapped
        // as Option<T>. This is used to parse a rustfmt.toml that doesn't
        // specify all properties of `Config`.
        // We first parse into `PartialConfig`, then create a default `Config`
        // and overwrite the properties with corresponding values from `PartialConfig`.
        #[derive(Clone)]
        #[allow(unreachable_pub)]
        pub struct PartialConfig {
            $(pub $i: Option<<$ty as StyleEditionDefault>::ConfigType>),+
        }

        impl PartialConfig {
            /// The options of the rustfmt.toml `file`, whose text `text` is and whose root table
            /// `root` is, each read by its type's `ConfigType::from_toml`. Every key is an option:
            /// `refused_options` refuses the file otherwise, before this runs.
            fn read_toml<A: ::std::alloc::Allocator>(
                root: ::blaze_toml::Item<'_, '_, A>,
                text: &str,
                file: &Path,
            ) -> Result<PartialConfig, ConfigError> {
                let mut parsed = PartialConfig { $($i: None),+ };
                for item in root.children() {
                    match item.key() {
                        $(
                            stringify!($i) => {
                                let value = <<$ty as StyleEditionDefault>::ConfigType as
                                    $crate::config::config_type::ConfigType>::from_toml(item)
                                    .map_err(|refusal| {
                                        ConfigError::option(file, text, stringify!($i), refusal)
                                    })?;
                                parsed.$i = Some(value);
                            }
                        )+
                        key => {
                            let (line, column) =
                                ::blaze_toml::line_col(text, item.key_span().start);
                            return Err(ConfigError::Refused {
                                file: file.to_owned(),
                                options: vec![format!(
                                    "  `{key}` (line {line}, column {column}): not an option of \
                                     rustfmt {} (the pinned nightly's)",
                                    $crate::RUSTFMT_VERSION
                                )],
                            });
                        }
                    }
                }
                Ok(parsed)
            }

            /// The options that are set, as one TOML table in the order they are declared.
            fn toml_map(&self) -> Result<::blaze_data::Map<::std::alloc::Global>, ToTomlError> {
                let mut map = ::blaze_data::Map::new_in(::std::alloc::Global);
                $(
                    if let Some(value) = &self.$i {
                        let value = $crate::config::config_type::ConfigType::to_toml(value)
                            .map_err(|why| ToTomlError::NoTomlForm {
                                option: stringify!($i),
                                why,
                            })?;
                        map.insert(
                            ::blaze_own::String::from_str_in(stringify!($i), ::std::alloc::Global),
                            value,
                        );
                    }
                )+
                Ok(map)
            }
        }

        // Macro hygiene won't allow us to make `set_$i()` methods on Config
        // for each item, so this struct is used to give the API to set values:
        // `config.set().option(false)`. It's pretty ugly. Consider replacing
        // with `config.set_option(false)` if we ever get a stable/usable
        // `concat_idents!()`.
        #[allow(unreachable_pub)]
        pub struct ConfigSetter<'a>(&'a mut Config);

        impl<'a> ConfigSetter<'a> {
            $(
            #[allow(unreachable_pub)]
            pub fn $i(&mut self, value: <$ty as StyleEditionDefault>::ConfigType) {
                (self.0).$i.2 = value;
                match stringify!($i) {
                    "max_width"
                    | "use_small_heuristics"
                    | "fn_call_width"
                    | "single_line_if_else_max_width"
                    | "single_line_let_else_max_width"
                    | "attr_fn_like_width"
                    | "struct_lit_width"
                    | "struct_variant_width"
                    | "array_width"
                    | "chain_width" => self.0.set_heuristics(),
                    "merge_imports" => self.0.set_merge_imports(),
                    "fn_args_layout" => self.0.set_fn_args_layout(),
                    "hide_parse_errors" => self.0.set_hide_parse_errors(),
                    "version" => self.0.set_version(),
                    &_ => (),
                }
            }
            )+
        }

        #[allow(unreachable_pub)]
        pub struct CliConfigSetter<'a>(&'a mut Config);

        impl<'a> CliConfigSetter<'a> {
            $(
            #[allow(unreachable_pub)]
            pub fn $i(&mut self, value: <$ty as StyleEditionDefault>::ConfigType) {
                (self.0).$i.2 = value;
                (self.0).$i.4 = true;
                match stringify!($i) {
                    "max_width"
                    | "use_small_heuristics"
                    | "fn_call_width"
                    | "single_line_if_else_max_width"
                    | "single_line_let_else_max_width"
                    | "attr_fn_like_width"
                    | "struct_lit_width"
                    | "struct_variant_width"
                    | "array_width"
                    | "chain_width" => self.0.set_heuristics(),
                    "merge_imports" => self.0.set_merge_imports(),
                    "fn_args_layout" => self.0.set_fn_args_layout(),
                    "hide_parse_errors" => self.0.set_hide_parse_errors(),
                    "version" => self.0.set_version(),
                    &_ => (),
                }
            }
            )+
        }

        // Query each option, returns true if the user set the option, false if
        // a default was used.
        #[allow(unreachable_pub)]
        pub struct ConfigWasSet<'a>(&'a Config);

        impl<'a> ConfigWasSet<'a> {
            $(
            #[allow(unreachable_pub)]
            pub fn $i(&self) -> bool {
                (self.0).$i.1
            }
            )+
        }

        // Query each option, returns true if the user set the option via a CLI flag,
        // false if a default was used.
        #[allow(unreachable_pub)]
        pub struct CliConfigWasSet<'a>(&'a Config);

        impl<'a> CliConfigWasSet<'a> {
            $(
            #[allow(unreachable_pub)]
            pub fn $i(&self) -> bool {
                (self.0).$i.4
            }
            )+
        }

        impl Config {
            $(
            #[allow(unreachable_pub)]
            pub fn $i(&self) -> <$ty as StyleEditionDefault>::ConfigType {
                self.$i.0.set(true);
                self.$i.2.clone()
            }
            )+

            #[allow(unreachable_pub)]
            pub(super) fn default_with_style_edition(style_edition: StyleEdition) -> Config {
                Config {
                    $(
                        $i: (
                                Cell::new(false),
                                false,
                                <$ty as StyleEditionDefault>::style_edition_default(
                                    style_edition
                                ),
                                $stb,
                                false,
                            ),
                    )+
                }
            }

            #[allow(unreachable_pub)]
            pub fn set(&mut self) -> ConfigSetter<'_> {
                ConfigSetter(self)
            }

            #[allow(unreachable_pub)]
            pub fn set_cli(&mut self) -> CliConfigSetter<'_> {
                CliConfigSetter(self)
            }

            #[allow(unreachable_pub)]
            pub fn was_set(&self) -> ConfigWasSet<'_> {
                ConfigWasSet(self)
            }

            #[allow(unreachable_pub)]
            pub fn was_set_cli(&self) -> CliConfigWasSet<'_> {
                CliConfigWasSet(self)
            }

            #[allow(unreachable_pub)]
            pub fn was_option_set(&self, key: &str) -> bool {
                match key {
                    $(
                        stringify!($i) => {
                            ConfigWasSet(self).$i()
                        }
                    )+
                    _ => false
                }
            }

            fn fill_from_parsed_config(mut self, parsed: PartialConfig, dir: &Path) -> Config {
            $(
                if let Some(option_value) = parsed.$i {
                    let option_stable = self.$i.3;
                    if $crate::config::config_type::is_stable_option_and_value(
                        stringify!($i), option_stable, &option_value
                    ) {
                        self.$i.1 = true;
                        self.$i.2 = option_value;
                    }
                }
            )+
                self.set_heuristics();
                self.set_ignore(dir);
                self.set_merge_imports();
                self.set_fn_args_layout();
                self.set_hide_parse_errors();
                self.set_version();
                self
            }

            pub(crate) fn is_valid_name(name: &str) -> bool {
                match name {
                    $(
                        stringify!($i) => true,
                    )+
                        _ => false,
                }
            }

            #[allow(unreachable_pub)]
            pub fn is_valid_key_val(key: &str, val: &str) -> bool {
                match key {
                    $(
                        stringify!($i) => {
                            val.parse::<<$ty as StyleEditionDefault>::ConfigType>().is_ok()
                        }
                    )+
                        _ => false,
                }
            }

            #[allow(unreachable_pub)]
            pub fn used_options(&self) -> PartialConfig {
                PartialConfig {
                    $(
                        $i: if self.$i.0.get() {
                                Some(self.$i.2.clone())
                            } else {
                                None
                            },
                    )+
                }
            }

            #[allow(unreachable_pub)]
            pub fn all_options(&self) -> PartialConfig {
                PartialConfig {
                    $(
                        $i: Some(self.$i.2.clone()),
                    )+
                }
            }

            #[allow(unreachable_pub)]
            pub fn override_value(&mut self, key: &str, val: &str)
            {
                match key {
                    $(
                        stringify!($i) => {
                            let value = val.parse::<<$ty as StyleEditionDefault>::ConfigType>()
                                .expect(
                                    &format!(
                                        "Failed to parse override for {} (\"{}\") as a {}",
                                        stringify!($i),
                                        val,
                                        stringify!(<$ty as StyleEditionDefault>::ConfigType)
                                    )
                                );

                            // Users are currently allowed to set unstable
                            // options/variants via the `--config` options override.
                            //
                            // There is ongoing discussion about how to move forward here:
                            // https://github.com/rust-lang/rustfmt/pull/5379
                            //
                            // For now, do not validate whether the option or value is stable,
                            // just always set it.
                            self.$i.1 = true;
                            self.$i.2 = value;
                        }
                    )+
                    _ => panic!("Unknown config key in override: {}", key)
                }

                match key {
                    "max_width"
                    | "use_small_heuristics"
                    | "fn_call_width"
                    | "single_line_if_else_max_width"
                    | "single_line_let_else_max_width"
                    | "attr_fn_like_width"
                    | "struct_lit_width"
                    | "struct_variant_width"
                    | "array_width"
                    | "chain_width" => self.set_heuristics(),
                    "merge_imports" => self.set_merge_imports(),
                    "fn_args_layout" => self.set_fn_args_layout(),
                    "hide_parse_errors" => self.set_hide_parse_errors(),
                    "version" => self.set_version(),
                    &_ => (),
                }
            }

            #[allow(unreachable_pub)]
            pub fn is_hidden_option(name: &str) -> bool {
                const HIDE_OPTIONS: [&str; 7] = [
                    "verbose",
                    "verbose_diff",
                    "file_lines",
                    "width_heuristics",
                    "merge_imports",
                    "fn_args_layout",
                    "hide_parse_errors"
                ];
                HIDE_OPTIONS.contains(&name)
            }

            #[allow(unreachable_pub)]
            pub fn print_docs(out: &mut impl Write, include_unstable: bool) {
                let style_edition = StyleEdition::Edition2015;
                use std::cmp;
                let max = 0;
                $( let max = cmp::max(max, stringify!($i).len()+1); )+
                let space_str = " ".repeat(max);
                writeln!(out, "Configuration Options:").unwrap();
                $(
                    if $stb || include_unstable {
                        let name_raw = stringify!($i);

                        if !Config::is_hidden_option(name_raw) {
                            let mut name_out = String::with_capacity(max);
                            for _ in name_raw.len()..max-1 {
                                name_out.push(' ')
                            }
                            name_out.push_str(name_raw);
                            name_out.push(' ');
                            let default_value = <$ty as StyleEditionDefault>::style_edition_default(
                                style_edition
                            );
                            let mut default_str = format!("{}", default_value);
                            if default_str.is_empty() {
                                default_str = String::from("\"\"");
                            }
                            writeln!(out,
                                    "{}{} Default: {}{}",
                                    name_out,
                                    <<$ty as StyleEditionDefault>::ConfigType>::doc_hint(),
                                    default_str,
                                    if !$stb { " (unstable)" } else { "" }).unwrap();
                            $(
                                writeln!(out, "{}{}", space_str, $dstring).unwrap();
                            )+
                            writeln!(out).unwrap();
                        }
                    }
                )+
            }

            /// Reduces the maximum width of the configuration.
            ///
            /// This method is intended to be used when creating a forked
            /// configuration for a particular formatting context in which the
            /// total available width needs to be reduced. This reduces the
            /// size of max_width, and all other properties affected by small
            /// heuristics, without treating those adjustments as overrides.
            ///
            /// As an example use case, this method is used when formatting
            /// arms within a declarative macro.
            #[allow(unreachable_pub)]
            pub fn reduce_max_width(&mut self, delta: usize) {
                self.adjust_max_width(-(delta.min(isize::MAX as usize) as isize));
            }

            /// Increases the maximum width of the configuration.
            ///
            /// This method is intended to be used when creating a forked
            /// configuration for a particular formatting context in which the
            /// total available width needs to be increased. This increases the
            /// size of max_width, and all other properties affected by small
            /// heuristics, without treating those adjustments as overrides.
            ///
            /// As an example use case, this method is used when formatting
            /// arms within a declarative macro.
            #[allow(unreachable_pub)]
            pub fn increase_max_width(&mut self, delta: usize) {
                self.adjust_max_width(delta.min(isize::MAX as usize) as isize);
            }

            /// Adjusts the maximum width of the configuration.
            ///
            /// This method is intended to be used when creating a forked
            /// configuration for a particular formatting context in which the
            /// total available width needs to be reduced. This adjusts the
            /// size of max_width, and all other properties affected by small
            /// heuristics, without treating those adjustments as overrides.
            ///
            /// As an example use case, this method is used when formatting
            /// arms within a declarative macro.
            fn adjust_max_width(&mut self, delta: isize) {
                // NB: we need to either saturate or report an error (or some kind of testable
                // warning). I picked to saturate here.
                let adjust = |value: usize| value.saturating_add_signed(delta);

                self.array_width.2 = adjust(self.array_width.2);
                self.attr_fn_like_width.2 = adjust(self.attr_fn_like_width.2);
                self.chain_width.2 = adjust(self.chain_width.2);
                self.fn_call_width.2 = adjust(self.fn_call_width.2);
                self.single_line_if_else_max_width.2 = adjust(self.single_line_if_else_max_width.2);
                self.single_line_let_else_max_width.2 =
                    adjust(self.single_line_let_else_max_width.2);
                self.struct_lit_width.2 = adjust(self.struct_lit_width.2);
                self.struct_variant_width.2 = adjust(self.struct_variant_width.2);

                self.max_width.2 = adjust(self.max_width.2);
                self.set_heuristics();
            }

            fn set_width_heuristics(&mut self, heuristics: WidthHeuristics) {
                let max_width = self.max_width.2;
                let get_width_value = |
                    was_set: bool,
                    override_value: usize,
                    heuristic_value: usize,
                    config_key: &str,
                | -> usize {
                    if !was_set {
                        return heuristic_value;
                    }
                    if override_value > max_width {
                        eprintln!(
                            "`{0}` cannot have a value that exceeds `max_width`. \
                            `{0}` will be set to the same value as `max_width`",
                            config_key,
                        );
                        return max_width;
                    }
                    override_value
                };

                let fn_call_width = get_width_value(
                    self.was_set().fn_call_width(),
                    self.fn_call_width.2,
                    heuristics.fn_call_width,
                    "fn_call_width",
                );
                self.fn_call_width.2 = fn_call_width;

                let attr_fn_like_width = get_width_value(
                    self.was_set().attr_fn_like_width(),
                    self.attr_fn_like_width.2,
                    heuristics.attr_fn_like_width,
                    "attr_fn_like_width",
                );
                self.attr_fn_like_width.2 = attr_fn_like_width;

                let struct_lit_width = get_width_value(
                    self.was_set().struct_lit_width(),
                    self.struct_lit_width.2,
                    heuristics.struct_lit_width,
                    "struct_lit_width",
                );
                self.struct_lit_width.2 = struct_lit_width;

                let struct_variant_width = get_width_value(
                    self.was_set().struct_variant_width(),
                    self.struct_variant_width.2,
                    heuristics.struct_variant_width,
                    "struct_variant_width",
                );
                self.struct_variant_width.2 = struct_variant_width;

                let array_width = get_width_value(
                    self.was_set().array_width(),
                    self.array_width.2,
                    heuristics.array_width,
                    "array_width",
                );
                self.array_width.2 = array_width;

                let chain_width = get_width_value(
                    self.was_set().chain_width(),
                    self.chain_width.2,
                    heuristics.chain_width,
                    "chain_width",
                );
                self.chain_width.2 = chain_width;

                let single_line_if_else_max_width = get_width_value(
                    self.was_set().single_line_if_else_max_width(),
                    self.single_line_if_else_max_width.2,
                    heuristics.single_line_if_else_max_width,
                    "single_line_if_else_max_width",
                );
                self.single_line_if_else_max_width.2 = single_line_if_else_max_width;

                let single_line_let_else_max_width = get_width_value(
                    self.was_set().single_line_let_else_max_width(),
                    self.single_line_let_else_max_width.2,
                    heuristics.single_line_let_else_max_width,
                    "single_line_let_else_max_width",
                );
                self.single_line_let_else_max_width.2 = single_line_let_else_max_width;
            }

            fn set_heuristics(&mut self) {
                let max_width = self.max_width.2;
                match self.use_small_heuristics.2 {
                    Heuristics::Default =>
                        self.set_width_heuristics(WidthHeuristics::scaled(max_width)),
                    Heuristics::Max => self.set_width_heuristics(WidthHeuristics::set(max_width)),
                    Heuristics::Off => self.set_width_heuristics(WidthHeuristics::null()),
                };
            }

            fn set_ignore(&mut self, dir: &Path) {
                self.ignore.2.add_prefix(dir);
            }

            fn set_merge_imports(&mut self) {
                if self.was_set().merge_imports() {
                    eprintln!(
                        "Warning: the `merge_imports` option is deprecated. \
                        Use `imports_granularity=\"Crate\"` instead"
                    );
                    if !self.was_set().imports_granularity() {
                        self.imports_granularity.2 = if self.merge_imports() {
                            ImportGranularity::Crate
                        } else {
                            ImportGranularity::Preserve
                        };
                    }
                }
            }

            fn set_fn_args_layout(&mut self) {
                if self.was_set().fn_args_layout() {
                    eprintln!(
                        "Warning: the `fn_args_layout` option is deprecated. \
                        Use `fn_params_layout`. instead"
                    );
                    if !self.was_set().fn_params_layout() {
                        self.fn_params_layout.2 = self.fn_args_layout();
                    }
                }
            }

            fn set_hide_parse_errors(&mut self) {
                if self.was_set().hide_parse_errors() {
                    eprintln!(
                        "Warning: the `hide_parse_errors` option is deprecated. \
                        Use `show_parse_errors` instead"
                    );
                    if !self.was_set().show_parse_errors() {
                        self.show_parse_errors.2 = self.hide_parse_errors();
                    }
                }
            }

            fn set_version(&mut self) {
                if !self.was_set().version() {
                    return;
                }

                eprintln!(
                    "Warning: the `version` option is deprecated. \
                    Use `style_edition=\"{0}\"` instead.",
                    match self.version.2 {
                        Version::One => "2015",
                        Version::Two => "2024",
                    }
                );

                if self.was_set().style_edition() || self.was_set_cli().style_edition() {
                    eprintln!(
                        "Warning: the deprecated `version` option was \
                        used in conjunction with the `style_edition` \
                        option which takes precedence. \
                        The value of the `version` option will be ignored."
                    );
                }
            }

            #[allow(unreachable_pub)]
            /// Returns `true` if the config key was explicitly set and is the default value.
            pub fn is_default(&self, key: &str) -> bool {
                let style_edition = StyleEdition::Edition2015;
                $(
                    let default_value = <$ty as StyleEditionDefault>::style_edition_default(
                        style_edition
                    );
                    if let stringify!($i) = key {
                        return self.$i.1 && self.$i.2 == default_value;
                    }
                 )+
                false
            }
        }

        // Template for the default configuration
        impl Default for Config {
            fn default() -> Config {
                Config::default_with_style_edition(StyleEdition::Edition2015)
            }
        }
    )
}

pub(crate) fn is_stable_option_and_value<T>(
    option_name: &str,
    option_stable: bool,
    option_value: &T,
) -> bool
where
    T: PartialEq + std::fmt::Debug + ConfigType,
{
    let nightly = crate::is_nightly_channel!();
    let variant_stable = option_value.stable_variant();
    match (nightly, option_stable, variant_stable) {
        // Stable with an unstable option
        (false, false, _) => {
            eprintln!(
                "Warning: can't set `{option_name} = {option_value:?}`, unstable features are only \
                       available in nightly channel."
            );
            false
        }
        // Stable with a stable option, but an unstable variant
        (false, true, false) => {
            eprintln!(
                "Warning: can't set `{option_name} = {option_value:?}`, unstable variants are only \
                       available in nightly channel."
            );
            false
        }
        // Nightly: everything allowed
        // Stable with stable option and variant: allowed
        (true, _, _) | (false, true, true) => true,
    }
}
