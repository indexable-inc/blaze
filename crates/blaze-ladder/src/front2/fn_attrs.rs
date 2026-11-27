//! A fn item's attributes that change typing ([`TypeAttrs`]), as the checker's
//! `World::attrs` asks them of a FN (not only of an ADT).
//!
//! Who asks: blaze-typeck's constant promotion (rules/promote.rs `promotable_callee`) asks the
//! attributes of every callee of a call under a shared borrow, `&f(..)`, to see whether it
//! carries `#[rustc_promotable]` ([`TypeAttrs::PROMOTABLE`]). The Reference, "Destructors",
//! "Constant promotion": "Promotion of a value expression to a `'static` slot occurs when the
//! expression could be written in a constant and borrowed ..". The pinned library marks
//! (library/core/src/task/wake.rs, nightly-2026-10-27, lines 197-207):
//! ```text
//!     #[rustc_promotable]
//!     #[stable(feature = "futures_api", since = "1.36.0")]
//!     #[rustc_const_stable(feature = "futures_api", since = "1.36.0")]
//!     pub const fn new(
//!         clone: unsafe fn(*const ()) -> RawWaker,
//!         ..
//!     ) -> Self {
//! ```
//! and the same on `RawWaker::new` (lines 52-56).
//!
//! STD-CENSUS-1 class 1 (48 of 57 std rungs, 2026-11-16): std's `panic_with_hook`
//! (library/std/src/panicking.rs line 773) writes
//! `default_hook(&PanicHookInfo::new(location, payload.get(), can_unwind, force_no_backtrace))`:
//! a shared borrow of a call of an INHERENT IMPL's fn (`impl<'a> PanicHookInfo<'a> { fn new }`,
//! library/std/src/panic.rs line 49). The world answered `attrs` for ADTs only: the fn's key is a
//! member key (keys.rs `member_key`, std's crate prefix), `Keys::library_in` answers `None` for a
//! member, and the answer was `Gap::Fill(FillError::UnknownKey(..))` ("the checker key .. is in
//! no row of the front end's key map"). Class 5 (s2-gen-01-dyn-traits, `println!("{}",
//! describe(&Sq(3.0)))`: `format_args!` borrows `describe(..)`) is the same question of a
//! MODULE-LEVEL fn, answered by wanting the fn as an ADT ("a fn (asked for as an ADT)").
//!
//! The answer is read once while the fn head is lowered and kept in [`FnHead::qualifiers`], so
//! a `#[cfg_attr(.., rustc_promotable)]` is resolved under its crate's cfg before the body asks
//! for it. No other attribute of a fn changes typing today; one that does is added here, beside
//! this one.

use blaze_typeck::{CostClass, CostContract, SyncCost, TypeAttrs};

use super::heads::FnHead;

/// The attribute that marks a fn whose call may be promoted.
pub const PROMOTABLE: &str = "rustc_promotable";

/// The attribute that marks a compiler intrinsic ([`TypeAttrs::INTRINSIC`]): blaze-typeck's
/// rules/reify.rs refuses it as a fn pointer by name (core/src/intrinsics/mod.rs: "The actual
/// implementations live in the compiler."). `rustc_intrinsic_must_be_overridden` contains the
/// word, and the head's lowered qualifiers carry the answer.
pub const INTRINSIC: &str = "rustc_intrinsic";

/// The attribute that gates a fn on CPU features ([`TypeAttrs::TARGET_FEATURE`]): blaze-typeck's
/// rules/target_feature.rs makes a safe such fn's pointer `unsafe` outside its features (the
/// Reference, "The `target_feature` attribute"; library/stdarch/crates/core_arch/src/arm_shared/
/// neon/generated.rs line 2041, `#[target_feature(enable = "neon")] pub fn vaddq_u8`). A
/// `cfg_attr(target_feature = ..)` also contains the word; the head's lowered qualifiers decide.
pub const TARGET_FEATURE: &str = "target_feature";

/// The attribute that marks a compile-time-only fn (`core::mem::type_info`, `TypeId::info`,
/// `blaze_std::own::raw`): callable only during constant evaluation, so consteval treats it
/// as a `const fn` (`FnQualifiers::CONST`).
pub const COMPTIME: &str = "rustc_comptime";

/// Whether an attribute's path, as written, is `blaze::cost` (spaces around `::` allowed).
pub fn is_cost_attr(path: &str) -> bool {
    path.split_once("::").is_some_and(|(tool, rest)| tool.trim() == "blaze" && rest.trim() == "cost")
}

/// Parses the parenthesized arguments of `#[blaze::cost(...)]` on a function item:
///
/// - Primary time complexity class (positional or `time = <class>`):
///   `O(1)`, `amortized O(1)`, `O(log)` | `O(log n)`, `O(n)`, `O(n log n)`, `O(n^2)`
/// - Optional space complexity class (`space = <class>`): defaults to `time` when omitted.
/// - Optional synchronization cost (`lock_free` | `sync = local`, `atomic` | `sync = atomic`,
///   `locks` | `sync = lock`).
/// - Optional trust escape hatch (`trusted = "non-empty reason"`).
///
/// Malformed, empty, unknown, or duplicate clauses return [`CostContract::INVALID`] so nothing
/// is silently ignored (Rule 3).
pub fn parse_fn_cost(args: &str) -> CostContract {
    let Some(inner) = args.trim().strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
        return CostContract::INVALID;
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return CostContract::INVALID;
    }
    let mut time: Option<CostClass> = None;
    let mut space: Option<CostClass> = None;
    let mut sync: Option<SyncCost> = None;
    let mut trusted = false;
    let mut saw_trusted = false;

    let bytes = inner.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut paren = 0u32;
    let mut in_str = false;
    let mut escaped = false;
    let mut any_clause = false;

    loop {
        let at_end = i == bytes.len();
        let at_comma = !at_end && !in_str && paren == 0 && bytes[i] == b',';
        if at_end || at_comma {
            if in_str || paren != 0 {
                return CostContract::INVALID;
            }
            let clause = inner[start..i].trim();
            if clause.is_empty() {
                // Allow a single trailing comma after at least one valid clause, refuse empty middle clauses.
                if at_end && any_clause {
                    break;
                }
                return CostContract::INVALID;
            }
            any_clause = true;
            if let Some(eq) = find_top_level_eq(clause) {
                let key = clause[..eq].trim();
                let val = clause[eq + 1..].trim();
                match key {
                    "time" => {
                        if time.is_some() {
                            return CostContract::INVALID;
                        }
                        let Some(c) = parse_cost_class(val) else {
                            return CostContract::INVALID;
                        };
                        time = Some(c);
                    }
                    "space" => {
                        if space.is_some() {
                            return CostContract::INVALID;
                        }
                        let Some(c) = parse_cost_class(val) else {
                            return CostContract::INVALID;
                        };
                        space = Some(c);
                    }
                    "sync" => {
                        if sync.is_some() {
                            return CostContract::INVALID;
                        }
                        let s = match val {
                            "local" => SyncCost::Local,
                            "atomic" => SyncCost::Atomic,
                            "lock" => SyncCost::Lock,
                            _ => return CostContract::INVALID,
                        };
                        sync = Some(s);
                    }
                    "trusted" => {
                        if saw_trusted || !is_nonempty_quoted_str(val) {
                            return CostContract::INVALID;
                        }
                        saw_trusted = true;
                        trusted = true;
                    }
                    _ => return CostContract::INVALID,
                }
            } else {
                match clause {
                    "lock_free" => {
                        if sync.is_some() {
                            return CostContract::INVALID;
                        }
                        sync = Some(SyncCost::Local);
                    }
                    "atomic" => {
                        if sync.is_some() {
                            return CostContract::INVALID;
                        }
                        sync = Some(SyncCost::Atomic);
                    }
                    "locks" => {
                        if sync.is_some() {
                            return CostContract::INVALID;
                        }
                        sync = Some(SyncCost::Lock);
                    }
                    _ => {
                        if time.is_some() {
                            return CostContract::INVALID;
                        }
                        let Some(c) = parse_cost_class(clause) else {
                            return CostContract::INVALID;
                        };
                        time = Some(c);
                    }
                }
            }
            if at_end {
                break;
            }
            i += 1;
            start = i;
            continue;
        }
        let b = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'(' => paren += 1,
                b')' => {
                    let Some(next) = paren.checked_sub(1) else {
                        return CostContract::INVALID;
                    };
                    paren = next;
                }
                _ => {}
            }
        }
        i += 1;
    }

    if time.is_none() && space.is_none() && sync.is_none() {
        return CostContract::INVALID;
    }
    let time_val = time.unwrap_or(CostClass::None);
    let space_val = space.unwrap_or(time_val);
    let sync_val = sync.unwrap_or(SyncCost::None);
    CostContract::new(time_val, space_val, sync_val, trusted)
}

/// Parses one complexity class spelling (`O(1)`, `amortized O(1)`, `O(log)`, `O(log n)`, `O(n)`,
/// `O(n log n)`, `O(n^2)`).
fn parse_cost_class(s: &str) -> Option<CostClass> {
    let s = s.trim();
    if let Some(after) = s.strip_prefix("amortized") {
        let rest = after.strip_prefix(char::is_whitespace)?.trim();
        let inner = strip_big_o(rest)?;
        return (inner == "1").then_some(CostClass::AmortizedO1);
    }
    let inner = strip_big_o(s)?;
    let mut parts = inner.split_ascii_whitespace();
    let first = parts.next()?;
    match (first, parts.next(), parts.next(), parts.next()) {
        ("1", None, None, None) => Some(CostClass::O1),
        ("log", None, None, None) | ("log", Some("n"), None, None) => Some(CostClass::OLogN),
        ("n", None, None, None) => Some(CostClass::ON),
        ("n", Some("log"), Some("n"), None) => Some(CostClass::ONLogN),
        ("n^2", None, None, None) => Some(CostClass::ON2),
        ("n", Some("^"), Some("2"), None) => Some(CostClass::ON2),
        ("n", Some("^2"), None, None) => Some(CostClass::ON2),
        ("n^", Some("2"), None, None) => Some(CostClass::ON2),
        _ => None,
    }
}

/// Strips `O(...)` (with optional whitespace before `(` and inside parens).
fn strip_big_o(s: &str) -> Option<&str> {
    let rest = s.strip_prefix('O')?.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner.trim())
}

/// Finds the first `=` at paren depth 0 outside string literals.
fn find_top_level_eq(s: &str) -> Option<usize> {
    let mut paren = 0u32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, b) in s.bytes().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'(' => paren += 1,
                b')' => paren = paren.saturating_sub(1),
                b'=' if paren == 0 => return Some(i),
                _ => {}
            }
        }
    }
    None
}

/// Whether `s` is a single Rust double-quoted string literal with non-whitespace content.
fn is_nonempty_quoted_str(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' {
        return false;
    }
    let mut i = 1usize;
    let mut has_non_ws = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    return false;
                }
                has_non_ws = true;
                i += 1;
            }
            b'"' => return i + 1 == bytes.len() && has_non_ws,
            b => {
                if !b.is_ascii_whitespace() {
                    has_non_ws = true;
                }
                i += 1;
            }
        }
    }
    false
}

/// The typing attributes of the fn whose head is `f`, already read while that head was made.
pub fn fn_type_attrs(f: &FnHead<'_>) -> TypeAttrs {
    f.qualifiers.typing().with_cost(f.cost)
}
