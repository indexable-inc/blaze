//! The lints, each a port of clippy's (its file says which), and what runs them on a node.

mod allow_attributes;
mod allow_attributes_without_reason;
mod arc_with_non_send_sync;
mod await_holding_invalid;
mod bool_assert_comparison;
mod box_collection;
mod byte_char_slices;
mod clone_on_copy;
mod clone_on_ref_ptr;
mod collapsible_if;
mod crate_in_macro_def;
mod dbg_macro;
pub(crate) mod disallowed;
mod empty_enums;
pub(crate) mod format;
mod format_collect;
mod get_first;
pub(crate) mod guards;
mod implicit_clone;
mod inefficient_to_string;
mod infinite_loop;
mod io_other_error;
pub(crate) mod iter_over_hash_type;
mod iter_overeager_cloned;
mod large_enum_variant;
mod large_futures;
pub(crate) mod large_stack_arrays;
pub(crate) mod large_stack_frames;
mod large_types_passed_by_value;
mod let_underscore_future;
mod let_underscore_must_use;
mod map_clone;
mod map_err_ignore;
mod map_unwrap_or;
mod match_like_matches_macro;
mod multiple_unsafe_ops_per_block;
mod mutex_atomic;
mod needless_bool;
mod needless_borrow;
mod needless_borrows_for_generic_args;
mod needless_pass_by_value;
mod needless_return;
mod new_without_default;
mod nonstandard_macro_braces;
mod panic_in_result_fn;
mod print_stderr;
mod print_stdout;
mod rc_buffer;
mod rc_mutex;
mod redundant_clone;
mod redundant_static_lifetimes;
mod significant_drop_in_scrutinee;
mod significant_drop_tightening;
mod single_char_add_str;
mod str_to_string;
mod todo;
mod undocumented_unsafe_blocks;
mod unnecessary_map_or;
mod unnecessary_to_owned;
mod unused_result_ok;
mod unwrap_in_result;
mod upper_case_acronyms;
mod useless_borrows_in_formatting;
mod useless_format;
mod useless_vec;
mod utils;
mod vec_box;

#[cfg(test)]
pub(crate) mod disallowed_tests;
#[cfg(test)]
pub(crate) mod drops_tests;
#[cfg(test)]
mod more_clone_tests;
#[cfg(test)]
mod more_rules_tests;
#[cfg(test)]
mod more_types_tests;

use blaze_ids::bits::BitArray;
use serde::{Deserialize, Serialize};
use syntax::{TextRange, TextSize, view as ast, view::AstNode as _};

use crate::{
    Level, blaze,
    check::Cx,
    level::{Levels, Lint, Target},
    registry::{self, Info, LintIndex, Named},
};

/// When rustc runs a lint's pass, which orders what it emits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Over the crate root's file before macros expand, and again after, as an early pass.
    PreExpansion,
    /// Over the expanded crate, before the late passes.
    Early,
    /// Over the crate's HIR.
    Late,
    /// After the late pass, from what it gathered (clippy's `check_crate_post`), in the order of
    /// where each is.
    AfterLate,
}

macro_rules! implemented {
    ($( $lint:ident = $name:literal, $pass:ident; )*) => {
        /// A lint implemented here, by its index among them; a set of them is an [`On`].
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub(crate) enum Implemented {
            $( $lint, )*
        }

        impl Implemented {
            /// Every lint implemented, in the order of their indices.
            pub(crate) const ALL: &[Implemented] = &[$( Implemented::$lint, )*];

            /// Its name, without `clippy::`.
            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $( Implemented::$lint => $name, )*
                }
            }

            /// The pass clippy runs it in.
            const fn pass(self) -> Pass {
                match self {
                    $( Implemented::$lint => Pass::$pass, )*
                }
            }
        }
    };
}

implemented! {
    AllowAttributes = "allow_attributes", Early;
    AllowAttributesWithoutReason = "allow_attributes_without_reason", Early;
    ArcWithNonSendSync = "arc_with_non_send_sync", Late;
    AwaitHoldingLock = "await_holding_lock", Late;
    AwaitHoldingRefcellRef = "await_holding_refcell_ref", Late;
    BoolAssertComparison = "bool_assert_comparison", Late;
    BoxCollection = "box_collection", Late;
    ByteCharSlices = "byte_char_slices", Late;
    CloneOnCopy = "clone_on_copy", Late;
    CloneOnRefPtr = "clone_on_ref_ptr", Late;
    CollapsibleIf = "collapsible_if", Late;
    CrateInMacroDef = "crate_in_macro_def", Early;
    DbgMacro = "dbg_macro", Late;
    DisallowedMacros = "disallowed_macros", Late;
    DisallowedMethods = "disallowed_methods", Late;
    DisallowedTypes = "disallowed_types", Late;
    EmptyEnums = "empty_enums", Late;
    FormatCollect = "format_collect", Late;
    GetFirst = "get_first", Late;
    ImplicitClone = "implicit_clone", Late;
    InefficientToString = "inefficient_to_string", Late;
    InfiniteLoop = "infinite_loop", Late;
    IoOtherError = "io_other_error", Late;
    IterOverHashType = "iter_over_hash_type", Late;
    IterOvereagerCloned = "iter_overeager_cloned", Late;
    LargeEnumVariant = "large_enum_variant", Late;
    LargeFutures = "large_futures", Late;
    LargeStackArrays = "large_stack_arrays", Late;
    LargeStackFrames = "large_stack_frames", Late;
    LargeTypesPassedByValue = "large_types_passed_by_value", Late;
    LetUnderscoreFuture = "let_underscore_future", Late;
    LetUnderscoreMustUse = "let_underscore_must_use", Late;
    MapClone = "map_clone", Late;
    MapErrIgnore = "map_err_ignore", Late;
    MapUnwrapOr = "map_unwrap_or", Late;
    MatchLikeMatchesMacro = "match_like_matches_macro", Late;
    MultipleUnsafeOpsPerBlock = "multiple_unsafe_ops_per_block", Late;
    MutexAtomic = "mutex_atomic", Late;
    MutexInteger = "mutex_integer", Late;
    NeedlessBool = "needless_bool", Late;
    NeedlessBorrow = "needless_borrow", Late;
    NeedlessBorrowsForGenericArgs = "needless_borrows_for_generic_args", Late;
    NeedlessPassByValue = "needless_pass_by_value", Late;
    NeedlessReturn = "needless_return", Late;
    NewWithoutDefault = "new_without_default", Late;
    NonstandardMacroBraces = "nonstandard_macro_braces", PreExpansion;
    PanicInResultFn = "panic_in_result_fn", Late;
    PrintStderr = "print_stderr", Late;
    PrintStdout = "print_stdout", Late;
    RcBuffer = "rc_buffer", Late;
    RcMutex = "rc_mutex", Late;
    RedundantClone = "redundant_clone", Late;
    RedundantStaticLifetimes = "redundant_static_lifetimes", Early;
    SignificantDropInScrutinee = "significant_drop_in_scrutinee", Late;
    SignificantDropTightening = "significant_drop_tightening", Late;
    SingleCharAddStr = "single_char_add_str", Late;
    StrToString = "str_to_string", Late;
    Todo = "todo", Late;
    UndocumentedUnsafeBlocks = "undocumented_unsafe_blocks", Late;
    UnnecessaryMapOr = "unnecessary_map_or", Late;
    UnnecessaryToOwned = "unnecessary_to_owned", Late;
    UnusedResultOk = "unused_result_ok", Late;
    UnwrapInResult = "unwrap_in_result", Late;
    UpperCaseAcronyms = "upper_case_acronyms", Late;
    UselessBorrowsInFormatting = "useless_borrows_in_formatting", Late;
    UselessFormat = "useless_format", Late;
    UselessVec = "useless_vec", AfterLate;
    VecBox = "vec_box", Late;
}

/// The first bit of [`blaze::Lint`]s in an [`On`]: they follow clippy's implemented lints.
pub(crate) const BLAZE_BITS: usize = Implemented::ALL.len();

/// Every bit an [`On`] has a lint for.
const ON_BITS: usize = BLAZE_BITS + blaze::Lint::ALL.len();

/// The words of an [`On`]: as many as the two lint tables need, so adding a lint grows it.
const ON_WORDS: usize = ON_BITS.div_ceil(64);

const _: () = assert!(ON_WORDS * 64 >= ON_BITS, "an `On` holds a bit for every lint");
const _: () = assert!(size_of::<On>() == 8 * ON_WORDS);

/// A set of lints: bit `i` for the [`Implemented`] lint of index `i`, and bits
/// [`BLAZE_BITS`]`..` for [`blaze::Lint`]s. Inline words sized by the lint tables, so copying,
/// comparing and hashing it is a few word operations; it has no bit past [`ON_BITS`]. Its
/// serialized form is its words, in order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct On(BitArray<ON_WORDS>);

impl On {
    pub(crate) const EMPTY: On = On(BitArray::<ON_WORDS>::EMPTY);

    /// Every lint, clippy's implemented ones and blaze's.
    pub(crate) const ALL: On = On(BitArray::<ON_WORDS>::all_below(ON_BITS));

    /// Every [`blaze::Lint`].
    pub(crate) const BLAZE: On = On::ALL.without(On(BitArray::<ON_WORDS>::all_below(BLAZE_BITS)));

    /// The set of the one lint of bit `at`.
    pub(crate) const fn bit(at: usize) -> On {
        assert!(at < ON_BITS, "a bit of no lint");
        On(BitArray::<ON_WORDS>::EMPTY.with_bit(at))
    }

    /// The lints of either.
    pub(crate) const fn or(self, other: On) -> On {
        On(self.0.or(&other.0))
    }

    /// These lints but `other`'s.
    pub(crate) const fn without(self, other: On) -> On {
        On(self.0.and_not(&other.0))
    }

    /// Whether some lint is in both: one AND per word.
    #[inline]
    pub(crate) fn intersects(self, other: On) -> bool {
        !self.0.is_disjoint(&other.0)
    }

    #[inline]
    pub(crate) fn is_empty(self) -> bool {
        self.0.is_empty()
    }
}

impl std::ops::BitOr for On {
    type Output = On;

    #[inline]
    fn bitor(self, other: On) -> On {
        self.or(other)
    }
}

impl std::ops::BitOrAssign for On {
    #[inline]
    fn bitor_assign(&mut self, other: On) {
        *self = self.or(other);
    }
}

/// The lints by name, clippy's then `blaze::`'s.
impl std::fmt::Debug for On {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut set = f.debug_set();
        for at in self.0.iter() {
            match at.checked_sub(BLAZE_BITS) {
                None => set.entry(&format_args!("{}", Implemented::ALL[at].name())),
                Some(at) => set.entry(&format_args!("blaze::{}", blaze::Lint::ALL[at].name())),
            };
        }
        set.finish()
    }
}

/// Each implemented lint's index in clippy's table, found when blaze compiles: a lint not of
/// clippy's is a compile error here.
const REGISTRY_INDEX: [LintIndex; Implemented::ALL.len()] = {
    let mut indices = [match registry::lint_index_const(Implemented::ALL[0].name()) {
        Some(it) => it,
        None => panic!("an implemented lint is not clippy's"),
    }; Implemented::ALL.len()];
    let mut i = 1;
    while i < Implemented::ALL.len() {
        indices[i] = match registry::lint_index_const(Implemented::ALL[i].name()) {
            Some(it) => it,
            None => panic!("an implemented lint is not clippy's"),
        };
        i += 1;
    }
    indices
};

/// For each lint of clippy's table, by its index, the index of the implemented lint it is,
/// or `u8::MAX`.
static BY_REGISTRY: [u8; registry::LINT_COUNT] = {
    let mut by_registry = [u8::MAX; registry::LINT_COUNT];
    let mut i = 0;
    while i < Implemented::ALL.len() {
        by_registry[REGISTRY_INDEX[i].get()] = i as u8;
        i += 1;
    }
    by_registry
};

impl Implemented {
    /// What clippy declares of it.
    #[inline]
    pub(crate) fn info(self) -> &'static Info {
        REGISTRY_INDEX[self as usize].info()
    }

    /// The implemented lint `lint` of clippy's is, if it is one.
    #[inline]
    pub(crate) fn of(lint: LintIndex) -> Option<Implemented> {
        Implemented::ALL.get(usize::from(BY_REGISTRY[lint.get()])).copied()
    }

    #[inline]
    pub(crate) const fn bit(self) -> On {
        On::bit(self as usize)
    }
}

/// Which of rustc's passes over a crate emits `lint`, in their order, for code in the root
/// file or not.
pub(crate) fn phase(lint: Implemented, in_root_file: bool) -> u8 {
    match lint.pass() {
        Pass::PreExpansion if in_root_file => 0,
        Pass::PreExpansion | Pass::Early => 1,
        Pass::AfterLate => 3,
        Pass::Late => 2,
    }
}

/// Every lint implemented, by name.
pub fn names() -> impl Iterator<Item = &'static str> {
    Implemented::ALL.iter().map(|it| it.name())
}

/// The lints the command line, or their default, leaves at a level other than `allow`.
pub(crate) fn command_line_on(levels: &Levels) -> On {
    let mut on = On::EMPTY;
    for &lint in Implemented::ALL {
        if levels.of(Lint::Clippy(lint.info()), &[]).level != Level::Allow {
            on |= lint.bit();
        }
    }
    for &lint in blaze::Lint::ALL {
        if levels.of(Lint::Blaze(lint), &[]).level != Level::Allow {
            on |= lint.bit();
        }
    }
    on
}

/// Each lint name `text` writes as a path, `clippy::<name>` or `blaze::<name>`, with where it
/// is and the lints it stands for: one lint, a group, `clippy::all` or `blaze::hot`. A name of
/// no lint implemented here is left out.
pub(crate) fn names_in(text: &str, mut found: impl FnMut(TextRange, On)) {
    let bytes = text.as_bytes();
    for tool in ["clippy", "blaze"] {
        for at in memchr::memmem::find_iter(bytes, tool) {
            if at > 0 && is_ident_byte(bytes[at - 1]) {
                continue;
            }
            let mut end = skip_whitespace(bytes, at + tool.len());
            if !bytes[end..].starts_with(b"::") {
                continue;
            }
            end = skip_whitespace(bytes, end + 2);
            let name_start = end;
            while end < bytes.len() && is_ident_byte(bytes[end]) {
                end += 1;
            }
            let name = &text[name_start..end];
            let lints = match tool {
                "clippy" => clippy_lints(name),
                _ => blaze_lints(name),
            };
            if !lints.is_empty() {
                found(TextRange::new(offset(at), offset(end)), lints);
            }
        }
    }
}

fn offset(at: usize) -> TextSize {
    TextSize::new(u32::try_from(at).unwrap_or(u32::MAX))
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn skip_whitespace(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && bytes[at].is_ascii_whitespace() {
        at += 1;
    }
    at
}

/// The lints `clippy::<name>` stands for.
fn clippy_lints(name: &str) -> On {
    match registry::named(name) {
        Some(Named::Lint(info)) => {
            Implemented::of(info.index()).map_or(On::EMPTY, Implemented::bit)
        }
        Some(Named::Group(group)) => clippy_where(|info| info.group == group),
        Some(Named::All) => clippy_where(|info| info.group.in_all()),
        Some(Named::Renamed(to)) => to.strip_prefix("clippy::").map_or(On::EMPTY, clippy_lints),
        Some(Named::Removed(_)) | None => On::EMPTY,
    }
}

fn clippy_where(covers: impl Fn(&Info) -> bool) -> On {
    Implemented::ALL.iter().filter(|it| covers(it.info())).fold(On::EMPTY, |on, it| on | it.bit())
}

/// The lints `blaze::<name>` stands for.
fn blaze_lints(name: &str) -> On {
    // `#![blaze::capabilities]` asks for a lint without naming it: the files that write it
    // have that lint on, as the files that write `#![warn(blaze::std_use)]` do.
    if name == crate::level::CAPABILITIES {
        return crate::level::capabilities_asks_for().bit();
    }
    Target::blaze(name).unwrap_or(On::EMPTY)
}

/// Whether the walk goes into what derives expand to: only lints that run there need it.
pub(crate) fn in_derives(cx: &Cx<'_, '_>) -> bool {
    cx.on(Implemented::EmptyEnums)
}

/// Whether the walk goes into what macro calls expand to, for the code handed to them: only
/// lints that run on code a macro can be handed need it.
pub(crate) fn in_calls(cx: &Cx<'_, '_>) -> bool {
    cx.on_any(On::ALL.without(Implemented::EmptyEnums.bit()))
}

/// The method names some lint of a method call looks at; any other call is linted by none.
const METHODS: [&str; 28] = [
    "map",
    "unwrap_or",
    "unwrap_or_default",
    "map_or",
    "clone",
    "get",
    "push_str",
    "insert_str",
    "to_owned",
    "to_string",
    "to_os_string",
    "to_path_buf",
    "to_vec",
    "into_owned",
    "collect",
    "map_err",
    "ok",
    "unwrap",
    "expect",
    "next",
    "last",
    "nth",
    "skip",
    "take",
    "flatten",
    "filter",
    "find",
    "count",
];

blaze_phf::phf_table!(static METHOD = &METHODS;);

/// Runs the lints of `node`'s kind on it: one `match` on the kind, and for a method call one
/// perfect hash of its name, to the lints that look at such a node, in the order
/// [`check_node_by_casts`] runs them.
pub(crate) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    use syntax::SyntaxKind as K;
    #[cfg(test)]
    if BY_CASTS.get() {
        return check_node_by_casts(cx, node);
    }
    if cx.expansion().is_some() {
        if let Some(it) = ast::Enum::cast(node) {
            empty_enums::check(cx, it);
        }
        return;
    }
    if matches!(node.kind(), K::PATH | K::METHOD_CALL_EXPR | K::ATTR) || node == cx.root() {
        disallowed::check(cx, node);
    }
    match node.kind() {
        K::STRUCT | K::TRAIT => {
            let Some(item) = ast::Item::cast(node) else { return };
            upper_case_acronyms::check(cx, item);
        }
        K::TYPE_ALIAS => {
            let Some(item) = ast::Item::cast(node) else { return };
            upper_case_acronyms::check(cx, item);
            box_collection::check(cx, node);
        }
        K::ENUM => {
            let Some(item) = ast::Item::cast(node) else { return };
            upper_case_acronyms::check(cx, item);
            let Some(it) = ast::Enum::cast(node) else { return };
            empty_enums::check(cx, it);
            large_enum_variant::check(cx, node);
        }
        K::IMPL => {
            let Some(it) = ast::Impl::cast(node) else { return };
            new_without_default::check(cx, it);
            undocumented_unsafe_blocks::check(cx, node);
        }
        K::FN => {
            let Some(it) = ast::Fn::cast(node) else { return };
            needless_return::check_fn(cx, it);
            needless_pass_by_value::check(cx, node);
            large_types_passed_by_value::check(cx, node);
            large_stack_frames::check(cx, node);
            significant_drop_tightening::check(cx, node);
            await_holding_invalid::check(cx, node);
            box_collection::check(cx, node);
            panic_in_result_fn::check(cx, it);
        }
        K::ATTR => {
            let Some(it) = ast::Attr::cast(node) else { return };
            allow_attributes::check(cx, it);
            allow_attributes_without_reason::check(cx, it);
        }
        K::LET_STMT => {
            let Some(it) = ast::LetStmt::cast(node) else { return };
            let_underscore_future::check(cx, it);
            let_underscore_must_use::check(cx, it);
            mutex_atomic::check_let(cx, it);
        }
        K::LOOP_EXPR => {
            let Some(ast::Expr::LoopExpr(it)) = ast::Expr::cast(node) else { return };
            infinite_loop::check(cx, it);
        }
        K::FOR_EXPR => {
            let Some(ast::Expr::ForExpr(it)) = ast::Expr::cast(node) else { return };
            iter_over_hash_type::check(cx, it);
        }
        K::CONST => {
            let Some(it) = ast::Const::cast(node) else { return };
            redundant_static_lifetimes::check_const(cx, it);
            box_collection::check(cx, node);
        }
        K::STATIC => {
            let Some(it) = ast::Static::cast(node) else { return };
            redundant_static_lifetimes::check_static(cx, it);
            box_collection::check(cx, node);
            mutex_atomic::check_static(cx, it);
        }
        K::RECORD_FIELD | K::TUPLE_FIELD => box_collection::check(cx, node),
        K::MACRO_CALL => {
            let Some(it) = ast::MacroCall::cast(node) else { return };
            nonstandard_macro_braces::check_call(cx, it);
            bool_assert_comparison::check(cx, it);
            useless_borrows_in_formatting::check(cx, it);
            useless_format::check(cx, it);
            useless_vec::check(cx, it);
            dbg_macro::check(cx, node);
            todo::check(cx, node);
            print_stdout::check(cx, node);
            print_stderr::check(cx, node);
        }
        K::MACRO_RULES => {
            let Some(it) = ast::MacroRules::cast(node) else { return };
            nonstandard_macro_braces::check_rules(cx, it);
            crate_in_macro_def::check(cx, it);
        }
        K::STMT_LIST => {
            let Some(it) = ast::StmtList::cast(node) else { return };
            needless_bool::check_block(cx, it);
        }
        K::IDENT_PAT => {
            let Some(it) = ast::IdentPat::cast(node) else { return };
            needless_borrow::check_ref_pat(cx, it);
        }
        K::IF_EXPR => {
            let Some(ast::Expr::IfExpr(it)) = ast::Expr::cast(node) else { return };
            needless_bool::check_if(cx, it);
            collapsible_if::check(cx, it);
            match_like_matches_macro::check_if_let(cx, it);
            significant_drop_in_scrutinee::check_if(cx, it);
        }
        K::MATCH_EXPR => {
            let Some(ast::Expr::MatchExpr(it)) = ast::Expr::cast(node) else { return };
            match_like_matches_macro::check_match(cx, it);
            significant_drop_in_scrutinee::check_match(cx, it);
        }
        K::WHILE_EXPR => {
            let Some(ast::Expr::WhileExpr(it)) = ast::Expr::cast(node) else { return };
            significant_drop_in_scrutinee::check_while(cx, it);
        }
        K::CALL_EXPR => {
            let Some(it) = ast::Expr::cast(node) else { return };
            io_other_error::check(cx, it);
            unwrap_in_result::check(cx, it);
            arc_with_non_send_sync::check(cx, it);
        }
        K::REF_EXPR => {
            let Some(it) = ast::Expr::cast(node) else { return };
            needless_borrow::check(cx, it);
            needless_borrows_for_generic_args::check(cx, it);
            byte_char_slices::check(cx, it);
        }
        K::ARRAY_EXPR | K::PAREN_EXPR => {
            let Some(it) = ast::Expr::cast(node) else { return };
            byte_char_slices::check(cx, it);
            large_stack_arrays::check(cx, it);
        }
        K::CLOSURE_EXPR => {
            let Some(ast::Expr::ClosureExpr(closure)) = ast::Expr::cast(node) else { return };
            needless_return::check_closure(cx, closure);
        }
        K::BLOCK_EXPR => {
            undocumented_unsafe_blocks::check(cx, node);
            multiple_unsafe_ops_per_block::check(cx, node);
        }
        K::METHOD_CALL_EXPR => check_method_call(cx, node),
        K::AWAIT_EXPR => {
            let Some(it) = ast::Expr::cast(node) else { return };
            large_futures::check(cx, it);
        }
        _ => {}
    }
}

/// The lints of a method call, by its name: those [`check_node_by_casts`] would run on it
/// that do not stop at its name.
fn check_method_call<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    let Some(method) = ast::MethodCallExpr::cast(node) else { return };
    let Some(name) = method.name_ref() else { return };
    let Some(index) = METHOD.get(name.text()) else { return };
    let expr = ast::Expr::MethodCallExpr(method);
    match index {
        0 => map_clone::check(cx, expr),
        1 | 2 => map_unwrap_or::check(cx, expr),
        3 => unnecessary_map_or::check(cx, expr),
        4 => {
            clone_on_copy::check(cx, expr);
            redundant_clone::check(cx, node);
            clone_on_ref_ptr::check(cx, node);
        }
        5 => get_first::check(cx, expr),
        6 | 7 => single_char_add_str::check(cx, expr),
        8 => {
            redundant_clone::check(cx, node);
            implicit_clone::check(cx, node);
            unnecessary_to_owned::check(cx, node);
        }
        9 => {
            redundant_clone::check(cx, node);
            unnecessary_to_owned::check(cx, node);
            inefficient_to_string::check(cx, node);
            str_to_string::check(cx, node);
        }
        10..=12 => {
            implicit_clone::check(cx, node);
            unnecessary_to_owned::check(cx, node);
        }
        13 => unnecessary_to_owned::check(cx, node),
        14 => format_collect::check(cx, node),
        15 => map_err_ignore::check(cx, expr),
        16 => unused_result_ok::check(cx, expr),
        17 | 18 => unwrap_in_result::check(cx, expr),
        19..=27 => iter_overeager_cloned::check(cx, expr),
        _ => {}
    }
}

#[cfg(test)]
thread_local! {
    /// Whether [`check_node`] runs [`check_node_by_casts`] instead, for the tests that compare
    /// the two.
    pub(crate) static BY_CASTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// [`check_node`] as it was before it dispatched on the kind: every lint asked, each casting
/// the node for itself. The reference the dispatch is tested against.
#[cfg(test)]
pub(crate) fn check_node_by_casts<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if cx.expansion().is_some() {
        if let Some(it) = ast::Enum::cast(node) {
            empty_enums::check(cx, it);
        }
        return;
    }
    if ast::Path::cast(node).is_some()
        || ast::MethodCallExpr::cast(node).is_some()
        || ast::Attr::cast(node).is_some()
        || node == cx.root()
    {
        disallowed::check(cx, node);
    }
    if let Some(it) = ast::Item::cast(node) {
        upper_case_acronyms::check(cx, it);
    }
    if let Some(it) = ast::Enum::cast(node) {
        empty_enums::check(cx, it);
    } else if let Some(it) = ast::Impl::cast(node) {
        new_without_default::check(cx, it);
    } else if let Some(it) = ast::Fn::cast(node) {
        needless_return::check_fn(cx, it);
        panic_in_result_fn::check(cx, it);
    } else if let Some(it) = ast::Const::cast(node) {
        redundant_static_lifetimes::check_const(cx, it);
    } else if let Some(it) = ast::Static::cast(node) {
        redundant_static_lifetimes::check_static(cx, it);
        mutex_atomic::check_static(cx, it);
    } else if let Some(it) = ast::MacroCall::cast(node) {
        nonstandard_macro_braces::check_call(cx, it);
        bool_assert_comparison::check(cx, it);
        useless_borrows_in_formatting::check(cx, it);
        useless_format::check(cx, it);
        useless_vec::check(cx, it);
    } else if let Some(it) = ast::MacroRules::cast(node) {
        nonstandard_macro_braces::check_rules(cx, it);
        crate_in_macro_def::check(cx, it);
    } else if let Some(it) = ast::StmtList::cast(node) {
        needless_bool::check_block(cx, it);
    } else if let Some(it) = ast::IdentPat::cast(node) {
        needless_borrow::check_ref_pat(cx, it);
    } else if let Some(it) = ast::Attr::cast(node) {
        allow_attributes::check(cx, it);
        allow_attributes_without_reason::check(cx, it);
    } else if let Some(it) = ast::LetStmt::cast(node) {
        let_underscore_future::check(cx, it);
        let_underscore_must_use::check(cx, it);
        mutex_atomic::check_let(cx, it);
    }
    if let Some(it) = ast::Expr::cast(node) {
        if let ast::Expr::IfExpr(it) = it {
            needless_bool::check_if(cx, it);
            collapsible_if::check(cx, it);
            match_like_matches_macro::check_if_let(cx, it);
            significant_drop_in_scrutinee::check_if(cx, it);
        }
        if let ast::Expr::MatchExpr(it) = it {
            match_like_matches_macro::check_match(cx, it);
            significant_drop_in_scrutinee::check_match(cx, it);
        }
        if let ast::Expr::WhileExpr(it) = it {
            significant_drop_in_scrutinee::check_while(cx, it);
        }
        io_other_error::check(cx, it);
        map_clone::check(cx, it);
        map_unwrap_or::check(cx, it);
        needless_borrow::check(cx, it);
        needless_borrows_for_generic_args::check(cx, it);
        unnecessary_map_or::check(cx, it);
        byte_char_slices::check(cx, it);
        large_stack_arrays::check(cx, it);
        clone_on_copy::check(cx, it);
        get_first::check(cx, it);
        single_char_add_str::check(cx, it);
        map_err_ignore::check(cx, it);
        unused_result_ok::check(cx, it);
        unwrap_in_result::check(cx, it);
        arc_with_non_send_sync::check(cx, it);
        iter_overeager_cloned::check(cx, it);
        large_futures::check(cx, it);
        if let ast::Expr::ClosureExpr(it) = it {
            needless_return::check_closure(cx, it);
        }
        if let ast::Expr::LoopExpr(it) = it {
            infinite_loop::check(cx, it);
        }
        if let ast::Expr::ForExpr(it) = it {
            iter_over_hash_type::check(cx, it);
        }
    }
    redundant_clone::check(cx, node);
    clone_on_ref_ptr::check(cx, node);
    implicit_clone::check(cx, node);
    unnecessary_to_owned::check(cx, node);
    inefficient_to_string::check(cx, node);
    format_collect::check(cx, node);
    str_to_string::check(cx, node);
    needless_pass_by_value::check(cx, node);
    large_types_passed_by_value::check(cx, node);
    large_stack_frames::check(cx, node);
    significant_drop_tightening::check(cx, node);
    await_holding_invalid::check(cx, node);
    box_collection::check(cx, node);
    large_enum_variant::check(cx, node);
    undocumented_unsafe_blocks::check(cx, node);
    multiple_unsafe_ops_per_block::check(cx, node);
    dbg_macro::check(cx, node);
    todo::check(cx, node);
    print_stdout::check(cx, node);
    print_stderr::check(cx, node);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implemented_lints_are_clippys() {
        assert_eq!(Implemented::ALL.len(), 68);
        for (at, &lint) in Implemented::ALL.iter().enumerate() {
            assert_eq!(lint as usize, at);
            let info = registry::lint(lint.name()).expect("a lint of clippy's");
            assert!(std::ptr::eq(lint.info(), info));
            assert_eq!(Implemented::of(info.index()), Some(lint));
            assert_eq!(lint.bit(), On::bit(at));
            assert!(!lint.bit().intersects(On::BLAZE));
        }
        let others = registry::lints()
            .iter()
            .filter(|it| Implemented::ALL.iter().all(|l| l.name() != it.name));
        for info in others {
            assert_eq!(Implemented::of(info.index()), None, "{}", info.name);
        }
        assert!(names().is_sorted());
    }

    #[test]
    fn a_method_name_is_found_at_its_place_in_the_table() {
        for (at, name) in METHODS.iter().enumerate() {
            assert_eq!(METHOD.get(name), Some(at), "{name}");
        }
        for name in ["", "clon", "clones", "Clone", "r#clone", "to_str", "iter", "unwrap_err"] {
            assert_eq!(METHOD.get(name), None, "{name}");
        }
    }

    /// The walk scans a node's children for attributes once, through `AnyHasAttrs`, and takes
    /// "none" to mean an item or ADT has none: so every item and ADT kind must be one.
    #[test]
    fn every_item_and_adt_kind_has_attrs() {
        use syntax::{
            SyntaxKind,
            view::{AnyHasAttrs, AstNode as _},
        };
        for raw in 0..SyntaxKind::__LAST as u16 {
            let kind = SyntaxKind::from(raw);
            if ast::Item::can_cast(kind) || ast::Adt::can_cast(kind) {
                assert!(AnyHasAttrs::can_cast(kind), "{kind:?}");
            }
        }
    }

    #[test]
    fn blazes_lints_take_the_bits_after_clippys() {
        for (at, &lint) in blaze::Lint::ALL.iter().enumerate() {
            assert_eq!(lint.bit(), On::bit(BLAZE_BITS + at));
            assert!(lint.bit().intersects(On::BLAZE));
        }
        for &lint in Implemented::ALL {
            assert!(!lint.bit().intersects(On::BLAZE));
        }
        let every = Implemented::ALL
            .iter()
            .map(|it| it.bit())
            .chain(blaze::Lint::ALL.iter().map(|it| it.bit()));
        assert_eq!(
            every.fold(On::EMPTY, |on, it| on | it),
            On::ALL,
            "one bit per lint, none past them"
        );
    }

    #[test]
    fn phases_by_pass() {
        assert_eq!(phase(Implemented::NonstandardMacroBraces, true), 0);
        assert_eq!(phase(Implemented::NonstandardMacroBraces, false), 1);
        assert_eq!(phase(Implemented::CrateInMacroDef, true), 1);
        assert_eq!(phase(Implemented::RedundantStaticLifetimes, false), 1);
        assert_eq!(phase(Implemented::NeedlessBool, true), 2);
        assert_eq!(phase(Implemented::UselessVec, false), 3);
    }

    /// The names of the lints of `on`, clippy's then blaze's.
    fn named(on: On) -> Vec<String> {
        let clippy = Implemented::ALL
            .iter()
            .filter(|it| on.intersects(it.bit()))
            .map(|it| it.name().to_owned());
        let mut blaze: Vec<String> = blaze::Lint::ALL
            .iter()
            .filter(|it| on.intersects(it.bit()))
            .map(|it| format!("blaze::{}", it.name()))
            .collect();
        blaze.sort();
        clippy.chain(blaze).collect()
    }

    fn named_in(text: &str) -> (On, Vec<TextRange>) {
        let (mut on, mut at) = (On::EMPTY, Vec::new());
        names_in(text, |range, lints| {
            on |= lints;
            at.push(range);
        });
        (on, at)
    }

    #[test]
    fn a_text_names_lints_groups_and_all_by_path() {
        let (on, at) = named_in("#![warn(clippy :: needless_bool)] fn f() {}");
        assert_eq!(named(on), ["needless_bool"]);
        assert_eq!(at, [TextRange::new(8.into(), 31.into())]);
        let (on, _) = named_in("#[deny(clippy::pedantic)] mod m {}");
        assert_eq!(on, clippy_where(|it| it.group == registry::Group::Pedantic));
        assert!(!on.is_empty());
        let (on, _) = named_in("#[allow(clippy::all)]");
        assert_eq!(on, clippy_where(|it| it.group.in_all()));
        assert!(!on.intersects(Implemented::DbgMacro.bit()), "a restriction lint is not in `all`");
        // `std_hashmap` is `default_hasher`'s old name.
        let (on, _) = named_in("#[expect(blaze::hot)] #[warn(blaze::std_hashmap)]");
        assert_eq!(
            named(on),
            [
                "blaze::cold_error",
                "blaze::default_hasher",
                "blaze::hot_alloc",
                "blaze::hot_alloc_call",
                "blaze::hot_clone",
                "blaze::hot_dyn",
                "blaze::implicit_allocator",
                "blaze::large_move",
            ]
        );
        let (general, _) = named_in("#[allow(blaze::general)]");
        let (internal, _) = named_in("#[allow(blaze::internal)]");
        assert!(
            general.intersects(blaze::Lint::HashTwice.bit())
                && !general.intersects(blaze::Lint::HotAlloc.bit())
        );
        assert!(
            internal.intersects(blaze::Lint::HotAlloc.bit())
                && !internal.intersects(blaze::Lint::HashTwice.bit())
        );
        let (pedantic, _) = named_in("#[allow(blaze::pedantic_perf)]");
        assert!(
            pedantic.intersects(blaze::Lint::DeepDropAtExit.bit())
                && !pedantic.intersects(blaze::Lint::HashTwice.bit())
        );
        let (edition, _) = named_in("#[warn(blaze::edition)]");
        let (macros, _) = named_in("#[allow(blaze::proc_macro)]");
        assert!(
            edition.intersects(blaze::Lint::BuildScriptAmbient.bit())
                && !edition.intersects(blaze::Lint::HashTwice.bit())
        );
        assert_eq!(
            named(macros),
            [
                "blaze::proc_macro_attribute",
                "blaze::proc_macro_derive",
                "blaze::proc_macro_fn_like",
            ],
            "`blaze::proc_macro` is the three macro lints, each of `blaze::edition`"
        );
        assert_eq!(macros.without(edition), On::EMPTY);
        assert!(edition.intersects(blaze::Lint::StdUse.bit()));
        let (capabilities, _) = named_in("#![blaze::capabilities]");
        assert_eq!(
            named(capabilities),
            ["blaze::std_use"],
            "`#![blaze::capabilities]` turns `std_use` on where it is written"
        );
        assert!(
            !(general.intersects(pedantic) || internal.intersects(pedantic)),
            "the kinds are disjoint"
        );
        assert!(
            !(general.intersects(edition)
                || internal.intersects(edition)
                || pedantic.intersects(edition)),
            "the kinds are disjoint"
        );
        assert_eq!(
            general | internal | pedantic | edition,
            On::BLAZE,
            "every blaze lint is of one kind"
        );
        for text in
            ["notclippy::needless_bool", "clippy::not_a_lint", "clippy needless_bool", "blaze::"]
        {
            assert_eq!(named_in(text).0, On::EMPTY, "{text:?}");
        }
    }

    #[test]
    fn the_command_line_turns_on_what_it_does_not_allow() {
        let flags = |args: &str| {
            let args: Vec<String> = args.split_whitespace().map(str::to_owned).collect();
            Levels::new(&crate::level::CommandLine::parse(&args).unwrap())
        };
        let default = command_line_on(&flags(""));
        assert!(default.intersects(Implemented::NeedlessBool.bit()));
        assert!(!default.intersects(Implemented::DbgMacro.bit()));
        assert!(
            default.intersects(blaze::Lint::HashTwice.bit()),
            "a general lint warns everywhere"
        );
        assert!(
            !default.intersects(crate::level::BLAZE_INTERNAL),
            "blaze's internal lints are allowed until turned on"
        );
        // Every check is on by default but restriction's: pedantic and nursery warn too.
        assert!(
            default.intersects(Implemented::NeedlessPassByValue.bit()),
            "pedantic warns by default"
        );
        assert!(default.intersects(Implemented::RedundantClone.bit()), "nursery warns by default");
        let without_all = command_line_on(&flags("-A clippy::all"));
        assert!(!without_all.intersects(clippy_where(|it| it.group.in_all())));
        assert!(
            without_all.intersects(Implemented::RedundantClone.bit()),
            "`clippy::all` is clippy's five groups only"
        );
        let on = command_line_on(&flags(&format!(
            "{} -A blaze::general -W clippy::dbg_macro -D blaze::hot_alloc",
            crate::tests::CLIPPY_OFF
        )));
        assert_eq!(named(on), ["dbg_macro", "blaze::hot_alloc"]);
    }
}
