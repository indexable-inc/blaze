//! The layout hash of an `-api` crate (AGENTS.md "COMPONENT TYPES LIVE IN -api CRATES; DEPENDENCY = CRATE HASH", docs/module-safety.md section 15).
//!
//! Every `-api` crate lists the layout types it exports ONCE, at the end of its `lib.rs`:
//!
//! ```ignore
//! hyperion_api_layouts::api_layouts! {
//!     "chat.Out" => Out,        // a component registered under a world name
//!     "svc.inventory" => InventoryApi,
//!     "mining.Dig" => alias Dig, // a second world name of a type listed above (no second `ApiComponent` impl)
//!     Slot,                     // a layout type with no world name (a field type, an enum, a record)
//!     data RIGS,                // a [`data_schema!`] row: a data file readers of this crate parse, with its marker
//! }
//! ```
//!
//! and gets `pub const LAYOUT_HASH: u64` (const, computed at compile time from every listed type's [`Layout::HASH`], size and align, the
//! hash of its recipe: field names, offsets, sizes, alignments; never source text, so a comment edit changes nothing) and
//! `pub static API_LAYOUTS: ApiLayouts` (the same list as data, for the mismatch message). The hash does not depend on the order of the list.
//!
//! A module that depends on the crate (a Cargo dependency, nothing hand-written) embeds `&API_LAYOUTS` in its manifest (`hyperion-module-build`
//! generates the table from the module's `Cargo.toml`, `hyperion_module!` includes it); the module that provides the behaviour embeds the same.
//! Every world-named entry also gets `impl ApiComponent` (the only way a typed query may name a type of another module, docs/module-architecture.md
//! 4.6); the module-private counterpart is [`LocalComponent`] (`local_component!`). See docs/module-contract.md for the exact layouts.
//! The loader refuses a consumer whose embedded hash differs from the provider's, printing both layouts (`hyperion-host/src/verify.rs`).
use hyperion_layout::{Layout, Recipe};
use hyperion_wyhash::Wyhash;

/// One layout type of an api crate.
#[derive(Debug)]
pub struct ApiType {
    /// Last path segment of the Rust type name.
    pub rust: &'static str,
    /// The world name it is registered under (`chat.Out`), empty when it is not a component of its own.
    pub world: &'static str,
    pub hash: u64,
    pub size: usize,
    pub align: usize,
    pub recipe: &'static Recipe,
    /// Migration chain of the component (`chain: [Old => New as auto]`), empty when it declares none.
    pub steps: &'static [hyperion_abi::table::Step],
}

/// One data file whose reader lives in an `-api` crate ([`data_schema!`]). `marker` is the 16 hex digits of the schema marker file as a number.
#[derive(Debug)]
pub struct ApiData {
    /// `"view.rigs"`.
    pub name: &'static str,
    /// The file name inside the generation's assets tree (`"rigs.json"`).
    pub file: &'static str,
    pub marker: u64,
    /// The reader type.
    pub rust: &'static str,
}

/// Everything an api crate exports as layouts, as embedded in a module image. Rust-only data: host and modules share one rustc (`BUILD_ID`).
#[derive(Debug)]
pub struct ApiLayouts {
    /// The Cargo package name (`hyperion-chat-api`).
    pub crate_name: &'static str,
    /// [`fold`] of `types` and `data`: the dependency hash.
    pub hash: u64,
    pub types: &'static [ApiType],
    pub data: &'static [ApiData],
}

/// A component type exported by an `-api` crate under a world name (emitted by [`api_layouts!`]). Typed queries and `get`/`set` accept only
/// `ApiComponent` and [`LocalComponent`] types, so a module cannot name a component of another module's implementation crate.
pub trait ApiComponent: Layout {
    /// The world name (`chat.Out`).
    const NAME: &'static str;
    /// The Cargo package of the `-api` crate that lists it (`hyperion-chat-api`).
    const API_CRATE: &'static str;
}

/// A module-private component (`<module>.*`), provided only by the image that declares it ([`local_component!`]).
pub trait LocalComponent: Layout {
    const NAME: &'static str;
    /// The Cargo package that declared it; `query!` const-asserts it equals the crate expanding the query ([`assert_local`]).
    const CRATE: &'static str;
}

/// Const check used by the typed query macro: `T` was declared by `krate` (`env!("CARGO_PKG_NAME")` of the crate expanding the macro).
pub const fn assert_local<T: LocalComponent>(krate: &str) {
    assert!(name_eq(T::CRATE, krate), "a LocalComponent of another crate cannot be used here: move the type into an -api crate");
}

/// Compile-time bound check: `T` comes from an `-api` crate.
pub const fn assert_api<T: ApiComponent>() {}

/// What a data reader type declares: the schema marker of the format it parses (16 hex digits; `docs/schemas/<x>.compat`).
pub trait DataSchema {
    const MARKER: &'static str;
}

/// Const parse of a schema marker (1 to 16 hex digits, surrounding ASCII whitespace ignored). Anything else fails to compile.
pub const fn marker_u64(s: &str) -> u64 {
    let b = s.as_bytes();
    let (mut i, mut end) = (0, b.len());
    while i < end && b[i].is_ascii_whitespace() {
        i += 1;
    }
    while end > i && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    assert!(end > i && end - i <= 16, "schema marker must be 1 to 16 hex digits");
    let mut v = 0u64;
    while i < end {
        let d = match b[i] {
            c @ b'0'..=b'9' => c - b'0',
            c @ b'a'..=b'f' => c - b'a' + 10,
            c @ b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("schema marker must be hex digits"),
        };
        v = (v << 4) | d as u64;
        i += 1;
    }
    v
}

/// Last `::` segment of a `stringify!`-ed type path (`lt_api::Table` becomes `Table`), so moving a type between modules of its crate changes nothing.
pub const fn last_segment(s: &'static str) -> &'static str {
    let b = s.as_bytes();
    let mut i = b.len();
    while i >= 2 {
        if b[i - 1] == b':' && b[i - 2] == b':' {
            return match s.split_at_checked(i) {
                Some((_, rest)) => rest,
                None => s,
            };
        }
        i -= 1;
    }
    s
}

pub const fn api_type<T: Layout>(rust: &'static str, world: &'static str) -> ApiType {
    ApiType { rust: last_segment(rust), world, hash: T::HASH, size: size_of::<T>(), align: align_of::<T>(), recipe: T::RECIPE, steps: T::CHAIN }
}

pub const fn api_data<T: DataSchema>(name: &'static str, file: &'static str, rust: &'static str) -> ApiData {
    ApiData { name, file, marker: marker_u64(T::MARKER), rust: last_segment(rust) }
}

/// The dependency hash: an order-independent sum of one hash per type (Rust name, layout hash, size, align) and one per data row (name, marker),
/// then the counts. A service type contributes its signature hash (`Service::LAYOUT`) as its `hash`.
pub const fn fold(types: &[ApiType], data: &[ApiData]) -> u64 {
    let mut sum = 0u64;
    let mut i = 0;
    while i < types.len() {
        let t = &types[i];
        let h = Wyhash::new(0).update(t.rust.as_bytes()).update_u64(t.hash).update_u64(t.size as u64).update_u64(t.align as u64).finish();
        sum = sum.wrapping_add(h);
        i += 1;
    }
    let mut j = 0;
    while j < data.len() {
        let d = &data[j];
        sum = sum.wrapping_add(Wyhash::new(1).update(d.name.as_bytes()).update_u64(d.marker).finish());
        j += 1;
    }
    Wyhash::new(0x4150_4948).update(b"api").update_u64(types.len() as u64).update_u64(data.len() as u64).update_u64(sum).finish()
}

/// Declare a data file whose reader lives in the `-api` crate: `data_schema!(RIGS: "view.rigs" => JFile, "rigs.json");` emits
/// `pub const RIGS: ApiData` (list it in [`api_layouts!`] as `data RIGS`). `JFile` implements [`DataSchema`] (its marker is the generated schema
/// marker). The marker becomes part of the api hash and a `ManifestData` row in every image that depends on the crate.
#[macro_export]
macro_rules! data_schema {
    ($(#[$m:meta])* $vis:vis $id:ident : $name:literal => $reader:ty, $file:literal) => {
        $(#[$m])*
        $vis const $id: $crate::ApiData = $crate::api_data::<$reader>($name, $file, stringify!($reader));
    };
}

/// Declare a module-private component's world name: `local_component!(Scratch => "locate.Scratch");` (the type already implements `Layout`
/// through `component!`). The crate that expands it is the only one that may use it in a typed query.
#[macro_export]
macro_rules! local_component {
    ($ty:ty => $name:literal) => {
        impl $crate::LocalComponent for $ty {
            const NAME: &'static str = $name;
            const CRATE: &'static str = env!("CARGO_PKG_NAME");
        }
    };
}

/// Declare the layout list of an `-api` crate (see the module docs): emits `LAYOUT_HASH`, `API_LAYOUTS` and an [`ApiComponent`] impl per
/// world-named entry.
#[macro_export]
macro_rules! api_layouts {
    (@munch [$($ty:expr,)*] [$(($w:literal, $it:ty))*] [$($d:ident)*] $(,)?) => {
        #[doc(hidden)]
        pub const __API_TYPES: &[$crate::ApiType] = &[ $($ty),* ];
        #[doc(hidden)]
        pub const __API_DATA: &[$crate::ApiData] = &[ $($d),* ];
        /// Hash of every layout and data marker this crate exports (`hyperion_api_layouts`): what a module that depends on the crate is built against.
        pub const LAYOUT_HASH: u64 = $crate::fold(__API_TYPES, __API_DATA);
        /// The same layouts as data (the loader prints both sides of a mismatch from it).
        pub static API_LAYOUTS: $crate::ApiLayouts =
            $crate::ApiLayouts { crate_name: env!("CARGO_PKG_NAME"), hash: LAYOUT_HASH, types: __API_TYPES, data: __API_DATA };
        $(
            impl $crate::ApiComponent for $it {
                const NAME: &'static str = $w;
                const API_CRATE: &'static str = env!("CARGO_PKG_NAME");
            }
        )*
    };
    (@munch [$($ty:expr,)*] $b:tt [$($d:ident)*] data $dn:ident , $($rest:tt)*) => {
        $crate::api_layouts!{@munch [$($ty,)*] $b [$($d)* $dn] $($rest)*}
    };
    (@munch [$($ty:expr,)*] $b:tt $c:tt $w:literal => alias $t:ty , $($rest:tt)*) => {
        $crate::api_layouts!{@munch [$($ty,)* $crate::api_type::<$t>(stringify!($t), $w),] $b $c $($rest)*}
    };
    (@munch [$($ty:expr,)*] [$($i:tt)*] $c:tt $w:literal => $t:ty , $($rest:tt)*) => {
        $crate::api_layouts!{@munch [$($ty,)* $crate::api_type::<$t>(stringify!($t), $w),] [$($i)* ($w, $t)] $c $($rest)*}
    };
    (@munch [$($ty:expr,)*] $b:tt $c:tt $t:ty , $($rest:tt)*) => {
        $crate::api_layouts!{@munch [$($ty,)* $crate::api_type::<$t>(stringify!($t), ""),] $b $c $($rest)*}
    };
    ($($t:tt)*) => { $crate::api_layouts!{@munch [] [] [] $($t)* ,} };
}

/// Const string equality (a macro asserts the literal service name equals `T::NAME`; [`assert_local`] compares crate names).
pub const fn name_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}
