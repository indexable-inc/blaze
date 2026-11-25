//! Built-in commands for `blaze-sh` (`bstd::sh`).
//!
//! Every builtin is pure sans-IO: filesystem, clock, process, system, and session effects go
//! through [`crate::protocol::Request`] via [`Evaluator::request`], and every allocation lives
//! in the caller's [`Alloc`].

use core::cmp::Ordering;
use core::fmt::Write as _;

use blaze_own::{ARc, AString, AVec, Alloc, Rc, Share};

use crate::ast::{BinaryOp, CallArg, CallExpr, Expr, ExprKind, PathKey};
use crate::error::{ErrorKind, ShellError};
use crate::machine::{Control, Evaluator};
use crate::persist::PVec;
use crate::protocol::{Answer, BuiltinSpec, FsOp, Request, StreamChunk};
use crate::span::Span;
use crate::value::{
    Bytes, ClosureData, DateTime, Nanos, PathMember, RangeVal, Record, Table, Value,
};

/// Single source of truth for `help` and editor tab-completion.
pub static BUILTIN_SPECS: &[BuiltinSpec] = &[
    // FS & OS
    BuiltinSpec {
        name: "ls",
        signature: "ls [-a|--all] [-l|--long] [path]",
        summary: "List directory entries as a structured table",
        flags: &["-a", "--all", "-l", "--long"],
    },
    BuiltinSpec {
        name: "cd",
        signature: "cd [path]",
        summary: "Change the current working directory",
        flags: &[],
    },
    BuiltinSpec {
        name: "pwd",
        signature: "pwd",
        summary: "Return the current working directory",
        flags: &[],
    },
    BuiltinSpec {
        name: "open",
        signature: "open [-r|--raw] <path>",
        summary: "Read a file, auto-parsing JSON, TOML, and CSV by extension",
        flags: &["-r", "--raw"],
    },
    BuiltinSpec {
        name: "save",
        signature: "save [-a|--append] [-r|--raw] <path>",
        summary: "Write the pipeline input value to a file",
        flags: &["-a", "--append", "-r", "--raw", "-f", "--force"],
    },
    BuiltinSpec {
        name: "cp",
        signature: "cp [-r|--recursive] <src> <dst>",
        summary: "Copy a file or directory",
        flags: &["-r", "--recursive"],
    },
    BuiltinSpec {
        name: "mv",
        signature: "mv <src> <dst>",
        summary: "Move or rename a file or directory",
        flags: &[],
    },
    BuiltinSpec {
        name: "rm",
        signature: "rm [-r|--recursive] [-f|--force] <path>...",
        summary: "Remove one or more files or directories",
        flags: &["-r", "--recursive", "-f", "--force"],
    },
    BuiltinSpec {
        name: "mkdir",
        signature: "mkdir [-p|--parents] <path>...",
        summary: "Create one or more directories",
        flags: &["-p", "--parents"],
    },
    BuiltinSpec {
        name: "touch",
        signature: "touch <path>...",
        summary: "Create empty files or update modification timestamps",
        flags: &[],
    },
    BuiltinSpec {
        name: "which",
        signature: "which <command>...",
        summary: "Locate a builtin, custom def, alias, or external executable",
        flags: &[],
    },
    BuiltinSpec {
        name: "ps",
        signature: "ps",
        summary: "List running OS processes as a table",
        flags: &[],
    },
    BuiltinSpec {
        name: "sys",
        signature: "sys",
        summary: "Return host OS, CPU, memory, and uptime info as a record",
        flags: &[],
    },
    BuiltinSpec {
        name: "date",
        signature: "date now",
        summary: "Date and time operations (subcommand: now)",
        flags: &[],
    },
    BuiltinSpec {
        name: "date now",
        signature: "date now",
        summary: "Return the current timestamp from the host clock",
        flags: &[],
    },
    BuiltinSpec {
        name: "env",
        signature: "env",
        summary: "Return the current environment variables as a sorted record",
        flags: &[],
    },
    // Table & List
    BuiltinSpec {
        name: "where",
        signature: "where <predicate|closure>",
        summary: "Filter rows of a table or items of a list by condition",
        flags: &[],
    },
    BuiltinSpec {
        name: "select",
        signature: "select <col|index>...",
        summary: "Select columns from a table/record or rows by index",
        flags: &[],
    },
    BuiltinSpec {
        name: "reject",
        signature: "reject <col|index>...",
        summary: "Remove columns from a table/record or rows by index",
        flags: &[],
    },
    BuiltinSpec {
        name: "get",
        signature: "get [-i|--ignore-errors] <path>",
        summary: "Extract a column, field, or index from a value",
        flags: &["-i", "--ignore-errors", "-o", "--optional"],
    },
    BuiltinSpec {
        name: "sort-by",
        signature: "sort-by [-r|--reverse] [-i|--ignore-case] [col|closure]...",
        summary: "Sort a table or list by column(s), closure, or natural order",
        flags: &["-r", "--reverse", "-i", "--ignore-case"],
    },
    BuiltinSpec {
        name: "reverse",
        signature: "reverse",
        summary: "Reverse a table, list, or string",
        flags: &[],
    },
    BuiltinSpec {
        name: "first",
        signature: "first [n]",
        summary: "Return the first element or the first n elements",
        flags: &[],
    },
    BuiltinSpec {
        name: "last",
        signature: "last [n]",
        summary: "Return the last element or the last n elements",
        flags: &[],
    },
    BuiltinSpec {
        name: "skip",
        signature: "skip <n>",
        summary: "Skip the first n rows or items",
        flags: &[],
    },
    BuiltinSpec {
        name: "take",
        signature: "take <n>",
        summary: "Take the first n rows or items (supports unbounded ranges)",
        flags: &[],
    },
    BuiltinSpec {
        name: "uniq",
        signature: "uniq [-c|--count]",
        summary: "Remove duplicate elements preserving first-seen order",
        flags: &["-c", "--count"],
    },
    BuiltinSpec {
        name: "length",
        signature: "length",
        summary: "Return the number of rows, elements, fields, or characters",
        flags: &[],
    },
    BuiltinSpec {
        name: "compact",
        signature: "compact [col]...",
        summary: "Drop nothing elements or rows containing nothing",
        flags: &[],
    },
    BuiltinSpec {
        name: "flatten",
        signature: "flatten [-a|--all] [col]...",
        summary: "Flatten nested lists or expand list columns in a table",
        flags: &["-a", "--all"],
    },
    BuiltinSpec {
        name: "enumerate",
        signature: "enumerate",
        summary: "Pair each item with its 0-based index in a table",
        flags: &[],
    },
    BuiltinSpec {
        name: "transpose",
        signature: "transpose [col_names...]",
        summary: "Transpose rows and columns of a table or record",
        flags: &[],
    },
    BuiltinSpec {
        name: "headers",
        signature: "headers",
        summary: "Promote the first row of a table or list-of-lists to column headers",
        flags: &[],
    },
    BuiltinSpec {
        name: "columns",
        signature: "columns",
        summary: "Return the column names of a table or keys of a record",
        flags: &[],
    },
    BuiltinSpec {
        name: "values",
        signature: "values",
        summary: "Return the values of a record or columns of a table",
        flags: &[],
    },
    BuiltinSpec {
        name: "append",
        signature: "append <value>",
        summary: "Append an element, list, record, or table to the end",
        flags: &[],
    },
    BuiltinSpec {
        name: "prepend",
        signature: "prepend <value>",
        summary: "Prepend an element, list, record, or table to the front",
        flags: &[],
    },
    BuiltinSpec {
        name: "zip",
        signature: "zip <other>",
        summary: "Combine two lists/tables element-wise into pairs",
        flags: &[],
    },
    BuiltinSpec {
        name: "each",
        signature: "each <closure>",
        summary: "Map a closure over each item (lifting records to a table)",
        flags: &[],
    },
    BuiltinSpec {
        name: "filter",
        signature: "filter <closure>",
        summary: "Keep items for which the closure returns truthy",
        flags: &[],
    },
    BuiltinSpec {
        name: "reduce",
        signature: "reduce [-f|--fold <init>] <closure>",
        summary: "Fold a list or table into a single value using a closure",
        flags: &["-f", "--fold"],
    },
    BuiltinSpec {
        name: "group-by",
        signature: "group-by [col|closure]",
        summary: "Group table rows or list items into a record of groups",
        flags: &[],
    },
    BuiltinSpec {
        name: "insert",
        signature: "insert <col> <value|closure>",
        summary: "Insert a field into a record or a column into a table",
        flags: &[],
    },
    BuiltinSpec {
        name: "update",
        signature: "update <col> <value|closure>",
        summary: "Update an existing field in a record or column in a table",
        flags: &[],
    },
    BuiltinSpec {
        name: "upsert",
        signature: "upsert <col> <value|closure>",
        summary: "Update an existing field/column or insert it if absent",
        flags: &[],
    },
    BuiltinSpec {
        name: "rename",
        signature: "rename <old> <new>",
        summary: "Rename a field in a record or a column in a table",
        flags: &[],
    },
    // Math
    BuiltinSpec {
        name: "math",
        signature: "math <sum|avg|min|max|median|abs|round|floor|ceil>",
        summary: "Numeric reductions and element-wise math functions",
        flags: &[],
    },
    BuiltinSpec {
        name: "math sum",
        signature: "math sum",
        summary: "Sum a list of numbers, sizes, or durations",
        flags: &[],
    },
    BuiltinSpec {
        name: "math avg",
        signature: "math avg",
        summary: "Compute the arithmetic mean of a list of numbers, sizes, or durations",
        flags: &[],
    },
    BuiltinSpec {
        name: "math min",
        signature: "math min",
        summary: "Return the minimum element of a list",
        flags: &[],
    },
    BuiltinSpec {
        name: "math max",
        signature: "math max",
        summary: "Return the maximum element of a list",
        flags: &[],
    },
    BuiltinSpec {
        name: "math median",
        signature: "math median",
        summary: "Compute the median of a list of numbers, sizes, or durations",
        flags: &[],
    },
    BuiltinSpec {
        name: "math abs",
        signature: "math abs",
        summary: "Absolute value of a number, size, duration, or list",
        flags: &[],
    },
    BuiltinSpec {
        name: "math round",
        signature: "math round",
        summary: "Round a number or list of numbers to the nearest integer",
        flags: &[],
    },
    BuiltinSpec {
        name: "math floor",
        signature: "math floor",
        summary: "Floor a number or list of numbers",
        flags: &[],
    },
    BuiltinSpec {
        name: "math ceil",
        signature: "math ceil",
        summary: "Ceiling of a number or list of numbers",
        flags: &[],
    },
    // String & Parsing
    BuiltinSpec {
        name: "str",
        signature: "str <trim|replace|split|join|upcase|downcase|contains|starts-with|ends-with|length|substring|index-of>",
        summary: "String manipulation commands",
        flags: &[],
    },
    BuiltinSpec {
        name: "str trim",
        signature: "str trim [-l|--left] [-r|--right] [-c|--char <ch>]",
        summary: "Trim whitespace or a specific character from a string",
        flags: &["-l", "--left", "-r", "--right", "-c", "--char", "-a", "--all"],
    },
    BuiltinSpec {
        name: "str replace",
        signature: "str replace [-a|--all] <find> <replace>",
        summary: "Replace first (or all) occurrences of a substring",
        flags: &["-a", "--all"],
    },
    BuiltinSpec {
        name: "str split",
        signature: "str split <sep>",
        summary: "Split a string by a separator into a list",
        flags: &[],
    },
    BuiltinSpec {
        name: "str join",
        signature: "str join [sep]",
        summary: "Join a list of values into a string",
        flags: &[],
    },
    BuiltinSpec {
        name: "str upcase",
        signature: "str upcase",
        summary: "Convert a string or list of strings to uppercase",
        flags: &[],
    },
    BuiltinSpec {
        name: "str downcase",
        signature: "str downcase",
        summary: "Convert a string or list of strings to lowercase",
        flags: &[],
    },
    BuiltinSpec {
        name: "str contains",
        signature: "str contains [-i|--ignore-case] <substr>",
        summary: "Check whether a string contains a substring",
        flags: &["-i", "--ignore-case"],
    },
    BuiltinSpec {
        name: "str starts-with",
        signature: "str starts-with <prefix>",
        summary: "Check whether a string starts with a prefix",
        flags: &[],
    },
    BuiltinSpec {
        name: "str ends-with",
        signature: "str ends-with <suffix>",
        summary: "Check whether a string ends with a suffix",
        flags: &[],
    },
    BuiltinSpec {
        name: "str length",
        signature: "str length",
        summary: "Return the character length of a string",
        flags: &[],
    },
    BuiltinSpec {
        name: "str substring",
        signature: "str substring <range|start end>",
        summary: "Extract a character-indexed slice of a string",
        flags: &[],
    },
    BuiltinSpec {
        name: "str index-of",
        signature: "str index-of <substr>",
        summary: "Return the index of a substring, or -1 if absent",
        flags: &[],
    },
    BuiltinSpec {
        name: "lines",
        signature: "lines",
        summary: "Split text into a list of lines",
        flags: &[],
    },
    BuiltinSpec {
        name: "split",
        signature: "split <row|column> <sep> [cols...]",
        summary: "Split text into rows or table columns",
        flags: &[],
    },
    BuiltinSpec {
        name: "split row",
        signature: "split row <sep>",
        summary: "Split a string into a list of rows by separator",
        flags: &[],
    },
    BuiltinSpec {
        name: "split column",
        signature: "split column <sep> [col_names...]",
        summary: "Split lines of text into a structured table by separator",
        flags: &[],
    },
    BuiltinSpec {
        name: "parse",
        signature: "parse <template>",
        summary: "Extract named `{field}` captures from text into a table",
        flags: &[],
    },
    // Conversions & Formats
    BuiltinSpec {
        name: "into",
        signature: "into <int|float|string|size|duration|bool|binary|datetime>",
        summary: "Convert a value to a target type",
        flags: &[],
    },
    BuiltinSpec {
        name: "into int",
        signature: "into int",
        summary: "Convert a value to an integer",
        flags: &[],
    },
    BuiltinSpec {
        name: "into float",
        signature: "into float",
        summary: "Convert a value to a floating-point number",
        flags: &[],
    },
    BuiltinSpec {
        name: "into string",
        signature: "into string",
        summary: "Convert a value to a string",
        flags: &[],
    },
    BuiltinSpec {
        name: "into size",
        signature: "into size",
        summary: "Convert a value to a byte size",
        flags: &[],
    },
    BuiltinSpec {
        name: "into duration",
        signature: "into duration",
        summary: "Convert a value to a duration",
        flags: &[],
    },
    BuiltinSpec {
        name: "into bool",
        signature: "into bool",
        summary: "Convert a value to a boolean",
        flags: &[],
    },
    BuiltinSpec {
        name: "into binary",
        signature: "into binary",
        summary: "Convert a value to raw binary bytes",
        flags: &[],
    },
    BuiltinSpec {
        name: "into datetime",
        signature: "into datetime",
        summary: "Convert a timestamp or ISO-8601 string to a datetime",
        flags: &[],
    },
    BuiltinSpec {
        name: "to",
        signature: "to <json|csv|toml|text>",
        summary: "Serialize a structured value to a text format",
        flags: &[],
    },
    BuiltinSpec {
        name: "to json",
        signature: "to json [-r|--raw]",
        summary: "Serialize a value to JSON text",
        flags: &["-r", "--raw"],
    },
    BuiltinSpec {
        name: "to csv",
        signature: "to csv",
        summary: "Serialize a table or record to RFC 4180 CSV text",
        flags: &[],
    },
    BuiltinSpec {
        name: "to toml",
        signature: "to toml",
        summary: "Serialize a record to TOML text",
        flags: &[],
    },
    BuiltinSpec {
        name: "to text",
        signature: "to text",
        summary: "Render a value as human-readable plain text",
        flags: &[],
    },
    BuiltinSpec {
        name: "from",
        signature: "from <json|csv|toml|lines>",
        summary: "Parse a text format into structured values",
        flags: &[],
    },
    BuiltinSpec {
        name: "from json",
        signature: "from json",
        summary: "Parse JSON text into structured values (lifting arrays of objects to tables)",
        flags: &[],
    },
    BuiltinSpec {
        name: "from csv",
        signature: "from csv [--noheaders] [-s|--separator <ch>]",
        summary: "Parse RFC 4180 CSV text into a structured table",
        flags: &["-n", "--noheaders", "-s", "--separator"],
    },
    BuiltinSpec {
        name: "from toml",
        signature: "from toml",
        summary: "Parse TOML text into a structured record",
        flags: &[],
    },
    BuiltinSpec {
        name: "from lines",
        signature: "from lines",
        summary: "Split text into a list of lines",
        flags: &[],
    },
    // Core / Meta / Session
    BuiltinSpec {
        name: "describe",
        signature: "describe",
        summary: "Describe the type or schema of the pipeline input",
        flags: &[],
    },
    BuiltinSpec {
        name: "echo",
        signature: "echo [args...]",
        summary: "Return its arguments as a value or list",
        flags: &[],
    },
    BuiltinSpec {
        name: "print",
        signature: "print [-n|--no-newline] [args...]",
        summary: "Stream formatted output to the host immediately",
        flags: &["-n", "--no-newline"],
    },
    BuiltinSpec {
        name: "do",
        signature: "do <closure> [args...]",
        summary: "Invoke a closure with positional arguments and pipeline input",
        flags: &[],
    },
    BuiltinSpec {
        name: "ignore",
        signature: "ignore",
        summary: "Discard pipeline input and return nothing",
        flags: &[],
    },
    BuiltinSpec {
        name: "help",
        signature: "help [command]",
        summary: "List all builtins as a table or inspect one command",
        flags: &[],
    },
    BuiltinSpec {
        name: "fork",
        signature: "fork [label]",
        summary: "Fork the current O(1) CoW session branch on the host",
        flags: &[],
    },
];

pub fn find_spec(name: &str) -> Option<&'static BuiltinSpec> {
    for spec in BUILTIN_SPECS {
        if spec.name == name {
            return Some(spec);
        }
    }
    None
}

pub fn is_builtin(name: &str) -> bool {
    if find_spec(name).is_some() {
        return true;
    }
    matches!(
        name,
        "into filesize" | "into date" | "date" | "math" | "str" | "into" | "to" | "from" | "split"
    )
}

// =============================================================================
// Pure no_std math & string helpers
// =============================================================================

#[inline]
pub(crate) fn f64_abs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & 0x7fff_ffff_ffff_ffff)
}

#[inline]
pub(crate) fn f64_trunc(x: f64) -> f64 {
    if !x.is_finite() || f64_abs(x) >= 4_503_599_627_370_496.0 {
        return x;
    }
    let t = (x as i64) as f64;
    if t == 0.0 && x.is_sign_negative() { -0.0 } else { t }
}

#[inline]
pub(crate) fn f64_floor(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let t = f64_trunc(x);
    if x < t { t - 1.0 } else { t }
}

#[inline]
pub(crate) fn f64_ceil(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let t = f64_trunc(x);
    if x > t { t + 1.0 } else { t }
}

#[inline]
pub(crate) fn f64_round(x: f64) -> f64 {
    if !x.is_finite() || f64_abs(x) >= 4_503_599_627_370_496.0 {
        return x;
    }
    let t = f64_trunc(x);
    let frac = f64_abs(x - t);
    if frac >= 0.5 {
        if x < 0.0 { t - 1.0 } else { t + 1.0 }
    } else {
        t
    }
}

pub(crate) fn f64_powi(mut base: f64, exp: i64) -> f64 {
    if exp == 0 {
        return 1.0;
    }
    let neg = exp < 0;
    let mut e = exp.unsigned_abs();
    let mut acc = 1.0f64;
    while e > 0 {
        if e & 1 == 1 {
            acc *= base;
        }
        e >>= 1;
        if e > 0 {
            base *= base;
        }
    }
    if neg { 1.0 / acc } else { acc }
}

fn f64_ln(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    // Extract binary exponent: x = m * 2^e with m in [1, 2).
    let bits = x.to_bits();
    let e_raw = ((bits >> 52) & 0x7ff) as i32;
    let (m, e) = if e_raw == 0 {
        // Subnormal: scale up by 2^54.
        let scaled = x * 18_014_398_509_481_984.0;
        let sb = scaled.to_bits();
        let se = ((sb >> 52) & 0x7ff) as i32 - 1023 - 54;
        let sm = f64::from_bits((sb & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
        (sm, se)
    } else {
        let m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
        (m, e_raw - 1023)
    };
    // Reduce m to [sqrt(0.5), sqrt(2)] for rapid convergence of atanh series.
    let (m, e) = if m > 1.414_213_562_373_095_1 { (m * 0.5, e + 1) } else { (m, e) };
    let y = (m - 1.0) / (m + 1.0);
    let y2 = y * y;
    let mut term = y;
    let mut sum = y;
    let mut denom = 3.0;
    for _ in 0..16 {
        term *= y2;
        sum += term / denom;
        denom += 2.0;
    }
    2.0 * sum + (e as f64) * core::f64::consts::LN_2
}

fn f64_exp(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x > 709.782_712_893_384 {
        return f64::INFINITY;
    }
    if x < -745.133_219_101_941_2 {
        return 0.0;
    }
    let k = f64_round(x / core::f64::consts::LN_2) as i32;
    let r = x - (k as f64) * core::f64::consts::LN_2;
    let mut term = 1.0f64;
    let mut sum = 1.0f64;
    for i in 1..=20 {
        term *= r / (i as f64);
        sum += term;
    }
    // Scale by 2^k safely.
    if k >= -1022 && k <= 1023 {
        let scale = f64::from_bits(((k + 1023) as u64) << 52);
        sum * scale
    } else if k < -1022 {
        let s1 = f64::from_bits(((k + 512 + 1023).max(1) as u64) << 52);
        let s2 = f64::from_bits(((-512 + 1023) as u64) << 52);
        sum * s1 * s2
    } else {
        let s1 = f64::from_bits(((k - 512 + 1023).min(2046) as u64) << 52);
        let s2 = f64::from_bits(((512 + 1023) as u64) << 52);
        sum * s1 * s2
    }
}

pub(crate) fn f64_powf(base: f64, exp: f64) -> f64 {
    if exp == 0.0 {
        return 1.0;
    }
    if base == 1.0 {
        return 1.0;
    }
    if base.is_nan() || exp.is_nan() {
        return f64::NAN;
    }
    if f64_trunc(exp) == exp && f64_abs(exp) < 1_000_000_000.0 {
        return f64_powi(base, exp as i64);
    }
    if base == 0.0 {
        return if exp > 0.0 { 0.0 } else { f64::INFINITY };
    }
    if base < 0.0 {
        return f64::NAN;
    }
    f64_exp(exp * f64_ln(base))
}

pub(crate) fn str_to_lowercase<'a>(s: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::with_capacity_in(s.len(), alloc);
    for ch in s.chars() {
        for lc in ch.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

pub(crate) fn str_to_uppercase<'a>(s: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::with_capacity_in(s.len(), alloc);
    for ch in s.chars() {
        for uc in ch.to_uppercase() {
            out.push(uc);
        }
    }
    out
}

fn str_replace_in<'a>(s: &str, find: &str, repl: &str, all: bool, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::with_capacity_in(s.len(), alloc);
    if find.is_empty() {
        if all {
            out.push_str(repl);
            for ch in s.chars() {
                out.push(ch);
                out.push_str(repl);
            }
        } else {
            out.push_str(repl);
            out.push_str(s);
        }
        return out;
    }
    let mut rest = s;
    while let Some(pos) = rest.find(find) {
        out.push_str(&rest[..pos]);
        out.push_str(repl);
        rest = &rest[pos + find.len()..];
        if !all {
            break;
        }
    }
    out.push_str(rest);
    out
}

fn ext_lowercase<'a>(path: &str, alloc: Alloc<'a>) -> Option<AString<'a>> {
    let file = match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    };
    let dot = file.rfind('.')?;
    if dot == 0 || dot + 1 >= file.len() {
        return None;
    }
    Some(str_to_lowercase(&file[dot + 1..], alloc))
}

// =============================================================================
// Pure Sans-IO Path Resolution
// =============================================================================

/// Resolve a user-supplied path against `cwd` and optional `home`, normalizing `.` and `..`.
pub(crate) fn resolve_path<'a>(
    cwd: &str,
    raw: &str,
    home: Option<&str>,
    alloc: Alloc<'a>,
) -> AString<'a> {
    let mut combined = AString::new_in(alloc);
    if raw == "~" {
        combined.push_str(home.unwrap_or(cwd));
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(h) = home {
            combined.push_str(h);
            combined.push('/');
            combined.push_str(rest);
        } else {
            combined.push_str(cwd);
            combined.push('/');
            combined.push_str(raw);
        }
    } else if raw.starts_with('/') {
        combined.push_str(raw);
    } else if raw.is_empty() {
        combined.push_str(cwd);
    } else {
        combined.push_str(cwd);
        if !cwd.ends_with('/') {
            combined.push('/');
        }
        combined.push_str(raw);
    }

    let mut parts: AVec<'a, &str> = AVec::new_in(alloc);
    for seg in combined.as_str().split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let mut out = AString::with_capacity_in(combined.len().max(1), alloc);
    if parts.is_empty() {
        out.push('/');
    } else {
        for seg in parts.iter() {
            out.push('/');
            out.push_str(seg);
        }
    }
    out
}

// =============================================================================
// JSON / TOML / CSV / Text Formatters & Parsers
// =============================================================================

pub(crate) fn to_json_string<'a>(
    val: &Value<'a>,
    compact: bool,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, ShellError<'a>> {
    let data = val.to_blaze_data(alloc);
    let opts = blaze_json::WriteOptions {
        style: if compact {
            blaze_json::Style::Compact
        } else {
            blaze_json::Style::Pretty { indent: 2 }
        },
        keys: blaze_json::KeyOrder::Insertion,
        bytes: blaze_json::BytesAs::Refuse,
    };
    let mut out = AString::new_in(alloc);
    blaze_json::write_into(&data, &mut out, opts).map_err(|e| {
        let mut msg = AString::new_in(alloc);
        let _ = write!(msg, "cannot serialize to JSON: {e}");
        ShellError::new(ErrorKind::Format, span, msg.as_str(), alloc)
    })?;
    Ok(out)
}

pub(crate) fn from_json_bytes<'a>(
    bytes: &[u8],
    span: Span,
    alloc: Alloc<'a>,
) -> Result<Value<'a>, ShellError<'a>> {
    let data = blaze_json::read_in(bytes, blaze_json::Limits::SHIPPED, alloc).map_err(|e| {
        let mut msg = AString::new_in(alloc);
        let _ = write!(msg, "invalid JSON at byte {}: {}", e.at, e.what);
        ShellError::new(ErrorKind::Format, span, msg.as_str(), alloc)
    })?;
    Ok(Value::from_blaze_data(&data, alloc))
}

pub(crate) fn from_toml_str<'a>(
    text: &str,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<Value<'a>, ShellError<'a>> {
    let doc =
        blaze_toml::Document::parse_in(text, blaze_toml::Limits::SHIPPED, alloc).map_err(|e| {
            let mut msg = AString::new_in(alloc);
            let _ = write!(msg, "invalid TOML at byte {}: {}", e.at, e.what);
            ShellError::new(ErrorKind::Format, span, msg.as_str(), alloc)
        })?;
    let data = doc.to_value_in(alloc);
    Ok(Value::from_blaze_data(&data, alloc))
}

pub(crate) fn to_toml_string<'a>(
    val: &Value<'a>,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, ShellError<'a>> {
    let data = val.to_blaze_data(alloc);
    let Some(map) = data.into_map() else {
        return Err(ShellError::new(
            ErrorKind::Type,
            span,
            "`to toml` requires a record as the root document",
            alloc,
        ));
    };
    let mut out = AString::new_in(alloc);
    blaze_toml::write_document(&map, &mut out, blaze_toml::DocumentOptions::CARGO).map_err(
        |e| {
            let mut msg = AString::new_in(alloc);
            let _ = write!(msg, "cannot serialize to TOML: {e}");
            ShellError::new(ErrorKind::Format, span, msg.as_str(), alloc)
        },
    )?;
    Ok(out)
}

/// RFC 4180 CSV parser into a columnar [`Table`].
pub(crate) fn parse_csv<'a>(
    text: &str,
    no_headers: bool,
    sep: char,
    alloc: Alloc<'a>,
) -> Table<'a> {
    let mut rows: AVec<'a, AVec<'a, AString<'a>>> = AVec::new_in(alloc);
    let mut cur_row: AVec<'a, AString<'a>> = AVec::new_in(alloc);
    let mut field = AString::new_in(alloc);
    let mut chars = text.chars().peekable();
    let mut in_quotes = false;
    let mut at_field_start = true;

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
        } else if at_field_start && ch == '"' {
            in_quotes = true;
            at_field_start = false;
        } else if ch == sep {
            cur_row.push(field);
            field = AString::new_in(alloc);
            at_field_start = true;
        } else if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            cur_row.push(field);
            field = AString::new_in(alloc);
            rows.push(cur_row);
            cur_row = AVec::new_in(alloc);
            at_field_start = true;
        } else if ch == '\n' {
            cur_row.push(field);
            field = AString::new_in(alloc);
            rows.push(cur_row);
            cur_row = AVec::new_in(alloc);
            at_field_start = true;
        } else {
            field.push(ch);
            at_field_start = false;
        }
    }
    if !at_field_start || !cur_row.is_empty() || !field.is_empty() {
        cur_row.push(field);
        rows.push(cur_row);
    }

    if rows.is_empty() {
        return Table::new(PVec::new(alloc), alloc);
    }

    let (headers, data_start) = if no_headers {
        let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut h = PVec::new(alloc);
        for i in 0..max_cols {
            let mut cname = AString::new_in(alloc);
            let _ = write!(cname, "column{i}");
            h.push(Rc::from_str_in(cname.as_str(), alloc));
        }
        (h, 0usize)
    } else {
        let mut h = PVec::new(alloc);
        for cell in rows[0].iter() {
            h.push(Rc::from_str_in(cell.as_str(), alloc));
        }
        (h, 1usize)
    };

    let mut table = Table::new(headers, alloc);
    for row in rows.iter().skip(data_start) {
        let mut vals = AVec::with_capacity_in(row.len(), alloc);
        for cell in row.iter() {
            vals.push(Value::string(cell.as_str(), alloc));
        }
        table.push_row_values(vals.as_slice());
    }
    table
}

fn write_csv_field<'a>(s: &str, out: &mut AString<'a>) {
    let needs_quote = s.bytes().any(|b| b == b',' || b == b'"' || b == b'\n' || b == b'\r');
    if !needs_quote {
        out.push_str(s);
        return;
    }
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
}

pub(crate) fn to_csv_string<'a>(
    val: &Value<'a>,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, ShellError<'a>> {
    let mut out = AString::new_in(alloc);
    match val {
        Value::Table(t) => {
            for c in 0..t.ncols() {
                if c > 0 {
                    out.push(',');
                }
                write_csv_field(t.columns.get(c).unwrap().as_ref(), &mut out);
            }
            out.push('\n');
            for r in 0..t.nrows() {
                for c in 0..t.ncols() {
                    if c > 0 {
                        out.push(',');
                    }
                    let cell = t.cell(r, c).unwrap().to_display_string(alloc);
                    write_csv_field(cell.as_str(), &mut out);
                }
                out.push('\n');
            }
            Ok(out)
        }
        Value::Record(rec) => {
            for i in 0..rec.len() {
                if i > 0 {
                    out.push(',');
                }
                write_csv_field(rec.keys.get(i).unwrap().as_ref(), &mut out);
            }
            out.push('\n');
            for i in 0..rec.len() {
                if i > 0 {
                    out.push(',');
                }
                let cell = rec.values.get(i).unwrap().to_display_string(alloc);
                write_csv_field(cell.as_str(), &mut out);
            }
            out.push('\n');
            Ok(out)
        }
        Value::List(items) => {
            if let Some(table) = Table::try_from_list(items, alloc) {
                return to_csv_string(&Value::Table(table), span, alloc);
            }
            for i in 0..items.len() {
                let cell = items.get(i).unwrap().to_display_string(alloc);
                write_csv_field(cell.as_str(), &mut out);
                out.push('\n');
            }
            Ok(out)
        }
        other => {
            let mut msg = AString::from_str_in("cannot convert ", alloc);
            msg.push_str(other.type_name());
            msg.push_str(" to CSV");
            Err(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc))
        }
    }
}

pub(crate) fn to_text_string<'a>(val: &Value<'a>, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    match val {
        Value::Table(t) => {
            for c in 0..t.ncols() {
                if c > 0 {
                    out.push('\t');
                }
                out.push_str(t.columns.get(c).unwrap().as_ref());
            }
            out.push('\n');
            for r in 0..t.nrows() {
                for c in 0..t.ncols() {
                    if c > 0 {
                        out.push('\t');
                    }
                    let _ = t.cell(r, c).unwrap().format_cell(&mut out);
                }
                out.push('\n');
            }
        }
        Value::Record(rec) => {
            for (k, v) in rec.iter() {
                out.push_str(k);
                out.push_str(": ");
                let _ = v.format_cell(&mut out);
                out.push('\n');
            }
        }
        Value::List(items) => {
            for i in 0..items.len() {
                if i > 0 {
                    out.push('\n');
                }
                let _ = items.get(i).unwrap().format_cell(&mut out);
            }
        }
        Value::Binary(b) => match core::str::from_utf8(b.as_ref()) {
            Ok(s) => out.push_str(s),
            Err(_) => {
                let _ = val.format_cell(&mut out);
            }
        },
        _ => {
            let _ = val.format_cell(&mut out);
        }
    }
    out
}

// =============================================================================
// Evaluated Call Arguments Helper
// =============================================================================

struct EvalArgs<'a> {
    pos: AVec<'a, (Value<'a>, Span)>,
    flags: AVec<'a, (ARc<'a, str>, bool, Option<Value<'a>>, Option<usize>, Span)>,
}

impl<'a> EvalArgs<'a> {
    fn eval(eval: &mut Evaluator<'a, '_>, args: &[CallArg<'a>]) -> Result<EvalArgs<'a>, Control<'a>> {
        let alloc = eval.alloc();
        let mut pos = AVec::new_in(alloc);
        let mut flags = AVec::new_in(alloc);
        for (i, arg) in args.iter().enumerate() {
            match arg {
                CallArg::Positional(expr) => {
                    let v = eval.eval_expr(expr, Value::Nothing)?;
                    pos.push((v, expr.span));
                }
                CallArg::Flag { name, short, value, span } => {
                    let v = match value {
                        Some(e) => Some(eval.eval_expr(e, Value::Nothing)?),
                        None => None,
                    };
                    let next_pos = if v.is_none() && matches!(args.get(i + 1), Some(CallArg::Positional(_))) {
                        Some(pos.len())
                    } else {
                        None
                    };
                    flags.push((name.share(), *short, v, next_pos, *span));
                }
            }
        }
        Ok(EvalArgs { pos, flags })
    }

    fn has_flag(&self, long: &str, short: &str) -> bool {
        for (name, is_short, _, _, _) in self.flags.iter() {
            if *is_short && !short.is_empty() && name.as_ref() == short {
                return true;
            }
            if !*is_short && !long.is_empty() && name.as_ref() == long {
                return true;
            }
            // Also tolerate combined short flags like `-rf`!
            if *is_short && !short.is_empty() && short.len() == 1 && name.as_ref().contains(short) {
                return true;
            }
        }
        false
    }

    fn flag_value(&self, long: &str, short: &str) -> Option<&Value<'a>> {
        for (name, is_short, val, _, _) in self.flags.iter() {
            let matched = if *is_short {
                !short.is_empty() && name.as_ref() == short
            } else {
                !long.is_empty() && name.as_ref() == long
            };
            if matched {
                return val.as_ref();
            }
        }
        None
    }

    /// Return a flag's value from either `--flag=val` or a space-separated `--flag val` / `-f val`.
    fn take_flag_value(&mut self, long: &str, short: &str) -> Option<Value<'a>> {
        let mut found_idx = None;
        for (idx, (name, is_short, val, next_pos, _)) in self.flags.iter().enumerate() {
            let matched = if *is_short {
                !short.is_empty() && name.as_ref() == short
            } else {
                !long.is_empty() && name.as_ref() == long
            };
            if matched {
                if let Some(v) = val {
                    return Some(v.share());
                }
                if let Some(pi) = next_pos {
                    found_idx = Some((idx, *pi));
                    break;
                }
            }
        }
        if let Some((fidx, pi)) = found_idx
            && pi < self.pos.len()
        {
            let (v, _) = self.pos.remove_shift(pi);
            self.flags[fidx].3 = None;
            for f in self.flags.iter_mut() {
                if let Some(ref mut np) = f.3
                    && *np > pi
                {
                    *np -= 1;
                }
            }
            return Some(v);
        }
        None
    }

    fn skip_first_pos(&mut self) {
        if !self.pos.is_empty() {
            self.pos.remove_shift(0);
            for f in self.flags.iter_mut() {
                if let Some(ref mut np) = f.3 {
                    if *np == 0 {
                        f.3 = None;
                    } else {
                        *np -= 1;
                    }
                }
            }
        }
    }
}

fn value_as_word<'a>(v: &Value<'a>, alloc: Alloc<'a>) -> Option<AString<'a>> {
    match v {
        Value::String(s) | Value::Path { path: s, .. } => Some(AString::from_str_in(s.as_ref(), alloc)),
        Value::CellPath(path) => {
            let mut out = AString::new_in(alloc);
            for (i, m) in path.iter().enumerate() {
                if i > 0 {
                    out.push('.');
                }
                match m {
                    PathMember::Field { name, .. } => out.push_str(name.as_ref()),
                    PathMember::Index { index, .. } => {
                        let _ = write!(out, "{index}");
                    }
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn value_as_col_name<'a>(v: &Value<'a>, span: Span, alloc: Alloc<'a>) -> Result<AString<'a>, ShellError<'a>> {
    match v {
        Value::String(s) => Ok(AString::from_str_in(s.as_ref(), alloc)),
        Value::CellPath(path) if path.len() == 1 => match &path[0] {
            PathMember::Field { name, .. } => Ok(AString::from_str_in(name.as_ref(), alloc)),
            PathMember::Index { .. } => Err(ShellError::new(
                ErrorKind::Type,
                span,
                "expected a column name, got an integer index",
                alloc,
            )),
        },
        other => {
            let mut msg = AString::from_str_in("expected a column name, got ", alloc);
            msg.push_str(other.type_name());
            Err(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc))
        }
    }
}

fn require_string<'a>(
    v: &Value<'a>,
    what: &str,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, ShellError<'a>> {
    if let Some(s) = value_as_word(v, alloc) {
        return Ok(s);
    }
    let mut msg = AString::from_str_in("expected string for ", alloc);
    msg.push_str(what);
    msg.push_str(", got ");
    msg.push_str(v.type_name());
    Err(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc))
}

fn require_int<'a>(v: &Value<'a>, what: &str, span: Span, alloc: Alloc<'a>) -> Result<i64, ShellError<'a>> {
    match v {
        Value::Int(n) => Ok(*n),
        other => {
            let mut msg = AString::from_str_in("expected int for ", alloc);
            msg.push_str(what);
            msg.push_str(", got ");
            msg.push_str(other.type_name());
            Err(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc))
        }
    }
}

fn require_closure<'a>(
    v: &Value<'a>,
    cmd: &str,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<ARc<'a, ClosureData<'a>>, ShellError<'a>> {
    match v {
        Value::Closure(c) => Ok(c.share()),
        other => {
            let mut msg = AString::from_str_in("`", alloc);
            msg.push_str(cmd);
            msg.push_str("` expects a closure, got ");
            msg.push_str(other.type_name());
            Err(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc))
        }
    }
}

fn arity_err<'a>(cmd: &str, expected: &str, span: Span, alloc: Alloc<'a>) -> Control<'a> {
    let mut msg = AString::from_str_in("`", alloc);
    msg.push_str(cmd);
    msg.push_str("`: ");
    msg.push_str(expected);
    Control::Error(ShellError::new(ErrorKind::Arity, span, msg.as_str(), alloc))
}

fn type_err<'a>(msg: &str, span: Span, alloc: Alloc<'a>) -> Control<'a> {
    Control::Error(ShellError::new(ErrorKind::Type, span, msg, alloc))
}

// =============================================================================
// Top-level Builtin Dispatcher
// =============================================================================

/// Dispatch `call` if it names a builtin command; returns `Ok(None)` if not a builtin.
pub(crate) fn dispatch_builtin<'a>(
    eval: &mut Evaluator<'a, '_>,
    call: &CallExpr<'a>,
    input: Value<'a>,
) -> Result<Option<Value<'a>>, Control<'a>> {
    let head = call.head.as_ref();
    let span = call.head_span;

    // `where` needs raw AST expressions so bare predicates like `where size > 1mb` work!
    if head == "where" {
        return builtin_where(eval, call, input).map(Some);
    }

    // Handle subcommand families whether parsed as `"math sum"` OR `"math"` + `"sum"`.
    if let Some((family, sub)) = split_builtin_head(head) {
        let args = EvalArgs::eval(eval, call.args.as_slice())?;
        return dispatch_subcommand(eval, family, sub, args, input, span).map(Some);
    }

    if matches!(head, "math" | "str" | "into" | "to" | "from" | "split" | "date") {
        let mut args = EvalArgs::eval(eval, call.args.as_slice())?;
        let alloc = eval.alloc();
        if args.pos.is_empty() {
            if head == "date" {
                return dispatch_subcommand(eval, "date", "now", args, input, span).map(Some);
            }
            return Err(arity_err(head, "missing subcommand", span, alloc));
        }
        let sub = value_as_word(&args.pos[0].0, alloc)
            .ok_or_else(|| type_err("subcommand must be a word", args.pos[0].1, alloc))?;
        args.skip_first_pos();
        return dispatch_subcommand(eval, head, sub.as_str(), args, input, span).map(Some);
    }

    if !is_builtin(head) {
        return Ok(None);
    }

    let args = EvalArgs::eval(eval, call.args.as_slice())?;
    let val = match head {
        // FS & OS
        "ls" => builtin_ls(eval, args, span)?,
        "cd" => builtin_cd(eval, args, span)?,
        "pwd" => Value::Path { path: eval.state().cwd.share(), kind: crate::value::PathKind::Dir },
        "open" => builtin_open(eval, args, span)?,
        "save" => builtin_save(eval, args, input, span)?,
        "cp" => builtin_cp(eval, args, span)?,
        "mv" => builtin_mv(eval, args, span)?,
        "rm" => builtin_rm(eval, args, span)?,
        "mkdir" => builtin_mkdir(eval, args, span)?,
        "touch" => builtin_touch(eval, args, span)?,
        "which" => builtin_which(eval, args, span)?,
        "ps" => builtin_ps(eval, span)?,
        "sys" => builtin_sys(eval, span)?,
        "env" => Value::Record(eval.state().env_record()),
        // Table & List
        "select" => builtin_select(eval, args, input, span)?,
        "reject" => builtin_reject(eval, args, input, span)?,
        "get" => builtin_get(eval, args, input, span)?,
        "sort-by" => builtin_sort_by(eval, args, input, span)?,
        "reverse" => builtin_reverse(eval, input, span)?,
        "first" => builtin_first(eval, args, input, span)?,
        "last" => builtin_last(eval, args, input, span)?,
        "skip" => builtin_skip(eval, args, input, span)?,
        "take" => builtin_take(eval, args, input, span)?,
        "uniq" => builtin_uniq(eval, args, input, span)?,
        "length" => builtin_length(eval, input, span)?,
        "compact" => builtin_compact(eval, args, input, span)?,
        "flatten" => builtin_flatten(eval, args, input, span)?,
        "enumerate" => builtin_enumerate(eval, input, span)?,
        "transpose" => builtin_transpose(eval, args, input, span)?,
        "headers" => builtin_headers(eval, input, span)?,
        "columns" => builtin_columns(eval, input, span)?,
        "values" => builtin_values(eval, input, span)?,
        "append" => builtin_append(eval, args, input, span)?,
        "prepend" => builtin_prepend(eval, args, input, span)?,
        "zip" => builtin_zip(eval, args, input, span)?,
        "each" => builtin_each(eval, args, input, span)?,
        "filter" => builtin_filter(eval, args, input, span)?,
        "reduce" => builtin_reduce(eval, args, input, span)?,
        "group-by" => builtin_group_by(eval, args, input, span)?,
        "insert" => builtin_insert_update_upsert(eval, args, input, span, UpsertMode::Insert)?,
        "update" => builtin_insert_update_upsert(eval, args, input, span, UpsertMode::Update)?,
        "upsert" => builtin_insert_update_upsert(eval, args, input, span, UpsertMode::Upsert)?,
        "rename" => builtin_rename(eval, args, input, span)?,
        // String & Parsing
        "lines" => builtin_lines(eval, input, span)?,
        "parse" => builtin_parse(eval, args, input, span)?,
        // Core / Meta / Session
        "describe" => builtin_describe(eval, input),
        "echo" => builtin_echo(eval, args),
        "print" => builtin_print(eval, args, input)?,
        "do" => builtin_do(eval, args, input, span)?,
        "ignore" => Value::Nothing,
        "help" => builtin_help(eval, args, span)?,
        "fork" => builtin_fork(eval, args, span)?,
        _ => return Ok(None),
    };
    Ok(Some(val))
}

fn split_builtin_head(head: &str) -> Option<(&str, &str)> {
    let idx = head.find(' ')?;
    let family = &head[..idx];
    let sub = &head[idx + 1..];
    if matches!(family, "math" | "str" | "into" | "to" | "from" | "split" | "date") {
        Some((family, sub))
    } else {
        None
    }
}

fn dispatch_subcommand<'a>(
    eval: &mut Evaluator<'a, '_>,
    family: &str,
    sub: &str,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match (family, sub) {
        ("date", "now") => match eval.request(Request::Now, span)? {
            Answer::Now(dt) => Ok(Value::Date(dt)),
            _ => Err(Control::Error(ShellError::new(
                ErrorKind::Io,
                span,
                "unexpected host answer for `date now`",
                alloc,
            ))),
        },
        ("math", s) => builtin_math(eval, s, input, span),
        ("str", s) => builtin_str(eval, s, args, input, span),
        ("split", "row") => builtin_split_row(eval, args, input, span),
        ("split", "column") => builtin_split_column(eval, args, input, span),
        ("into", s) => builtin_into(eval, s, input, span),
        ("to", s) => builtin_to(eval, s, args, input, span),
        ("from", s) => builtin_from(eval, s, args, input, span),
        _ => {
            let mut msg = AString::from_str_in("unknown subcommand `", alloc);
            msg.push_str(family);
            msg.push(' ');
            msg.push_str(sub);
            msg.push('`');
            Err(Control::Error(ShellError::new(ErrorKind::Undefined, span, msg.as_str(), alloc)))
        }
    }
}

// =============================================================================
// 1. FS & OS Builtins
// =============================================================================

fn builtin_ls<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let show_hidden = args.has_flag("all", "a");
    let long = args.has_flag("long", "l");
    let cwd = eval.state().cwd.share();
    let home = eval.state().get_env("HOME").map(|s| AString::from_str_in(s, alloc));
    let path = if let Some((v, s)) = args.pos.first() {
        let raw = require_string(v, "path", *s, alloc).map_err(Control::Error)?;
        resolve_path(cwd.as_ref(), raw.as_str(), home.as_ref().map(|h| h.as_str()), alloc)
    } else {
        AString::from_str_in(cwd.as_ref(), alloc)
    };
    let ans = eval.request(Request::ReadDir { path, show_hidden }, span)?;
    let Answer::DirEntries(entries) = ans else {
        return Err(Control::Error(ShellError::new(
            ErrorKind::Io,
            span,
            "unexpected host answer for `ls`",
            alloc,
        )));
    };
    let mut table = if long {
        Table::from_col_names(&["name", "type", "size", "modified", "readonly"], alloc)
    } else {
        Table::from_col_names(&["name", "type", "size", "modified"], alloc)
    };
    table.default_grid = !long;
    table.set_col_display("name", crate::value::ColDisplay::FileEntry);
    table.set_col_display("size", crate::value::ColDisplay::FileSize);
    table.set_col_display("modified", crate::value::ColDisplay::HumanTime);
    if long {
        table.set_col_display("readonly", crate::value::ColDisplay::Status);
    }
    // Sort directories first, then case-insensitive by name (like `eza --group-directories-first`).
    let mut order: AVec<'a, usize> = AVec::with_capacity_in(entries.len(), alloc);
    for i in 0..entries.len() {
        order.push(i);
    }
    order.sort_unstable_by(|&a, &b| {
        let ea = &entries[a];
        let eb = &entries[b];
        let da = !matches!(ea.kind, crate::protocol::FileKind::Dir);
        let db = !matches!(eb.kind, crate::protocol::FileKind::Dir);
        da.cmp(&db).then_with(|| {
            ea.name
                .as_bytes()
                .iter()
                .map(u8::to_ascii_lowercase)
                .cmp(eb.name.as_bytes().iter().map(u8::to_ascii_lowercase))
                .then_with(|| ea.name.as_str().cmp(eb.name.as_str()))
        })
    });
    for &idx in order.iter() {
        let e = &entries[idx];
        eval.charge(1, span)?;
        let type_str = e
            .analysis
            .as_ref()
            .map(|a| a.as_str())
            .unwrap_or(e.kind.as_str());
        if long {
            let row = [
                Value::string(e.name.as_str(), alloc),
                Value::string(type_str, alloc),
                Value::Size(e.size),
                Value::Date(e.modified),
                Value::Bool(e.readonly),
            ];
            table.push_row_values(&row);
        } else {
            let row = [
                Value::string(e.name.as_str(), alloc),
                Value::string(type_str, alloc),
                Value::Size(e.size),
                Value::Date(e.modified),
            ];
            table.push_row_values(&row);
        }
    }
    Ok(Value::Table(table))
}

fn builtin_cd<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let cwd = eval.state().cwd.share();
    let home = eval.state().get_env("HOME").map(|s| AString::from_str_in(s, alloc));
    let target = if let Some((v, s)) = args.pos.first() {
        let raw = require_string(v, "path", *s, alloc).map_err(Control::Error)?;
        if raw.as_str() == "-" {
            match eval.state().get_env("OLDPWD") {
                Some(old) => AString::from_str_in(old, alloc),
                None => {
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::Undefined,
                        span,
                        "OLDPWD is not set",
                        alloc,
                    )));
                }
            }
        } else if raw.as_str() == "~" || raw.as_str().is_empty() {
            match &home {
                Some(h) => AString::from_str_in(h.as_str(), alloc),
                None => {
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::Undefined,
                        span,
                        "HOME environment variable is not set",
                        alloc,
                    )));
                }
            }
        } else {
            resolve_path(cwd.as_ref(), raw.as_str(), home.as_ref().map(|h| h.as_str()), alloc)
        }
    } else {
        match &home {
            Some(h) => AString::from_str_in(h.as_str(), alloc),
            None => {
                return Err(Control::Error(ShellError::new(
                    ErrorKind::Undefined,
                    span,
                    "HOME environment variable is not set",
                    alloc,
                )));
            }
        }
    };

    let ans = eval.request(Request::ChangeDir { path: target }, span)?;
    let Answer::ChangedDir(new_cwd) = ans else {
        return Err(Control::Error(ShellError::new(
            ErrorKind::Io,
            span,
            "unexpected host answer for `cd`",
            alloc,
        )));
    };
    let old = eval.state().cwd.share();
    eval.state_mut().set_env("OLDPWD", old.as_ref());
    eval.state_mut().set_cwd(new_cwd.as_str());
    Ok(Value::Nothing)
}

fn builtin_open<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let raw_mode = args.has_flag("raw", "r");
    let Some((v, s)) = args.pos.first() else {
        return Err(arity_err("open", "missing file path", span, alloc));
    };
    let raw_path = require_string(v, "path", *s, alloc).map_err(Control::Error)?;
    let cwd = eval.state().cwd.share();
    let home = eval.state().get_env("HOME").map(|h| AString::from_str_in(h, alloc));
    let path = resolve_path(cwd.as_ref(), raw_path.as_str(), home.as_ref().map(|h| h.as_str()), alloc);
    let ext = ext_lowercase(path.as_str(), alloc);

    let path_for_sniff = path.clone();
    let ans = eval.request(Request::ReadFile { path }, span)?;
    let Answer::FileBytes(bytes) = ans else {
        return Err(Control::Error(ShellError::new(
            ErrorKind::Io,
            span,
            "unexpected host answer for `open`",
            alloc,
        )));
    };

    if !raw_mode {
        let sniff = blaze_sniff::sniff_file(path_for_sniff.as_str(), bytes.as_slice());
        match sniff.label {
            "JSON" | "Node manifest" | "TS config" | "Nix lock" => {
                return from_json_bytes(bytes.as_slice(), span, alloc).map_err(Control::Error);
            }
            "JSONL" => {
                if let Ok(text) = core::str::from_utf8(bytes.as_slice()) {
                    let mut list = PVec::new(alloc);
                    for line in text.lines() {
                        let t = line.trim();
                        if t.is_empty() {
                            continue;
                        }
                        if let Ok(val) = from_json_bytes(t.as_bytes(), span, alloc) {
                            list.push(val);
                        }
                    }
                    if let Some(tbl) = Table::try_from_list(&list, alloc) {
                        return Ok(Value::Table(tbl));
                    }
                    if !list.is_empty() {
                        return Ok(Value::List(list));
                    }
                }
            }
            "TOML" | "Cargo manifest" | "Cargo lock" | "uv lock" | "Blaze lock"
            | "Clippy config" | "Rustfmt config" | "Python proj" | "Rust pin" => {
                let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| {
                    Control::Error(ShellError::new(
                        ErrorKind::Format,
                        span,
                        "TOML file is not valid UTF-8",
                        alloc,
                    ))
                })?;
                return from_toml_str(text, span, alloc).map_err(Control::Error);
            }
            "CSV" | "TSV" => {
                let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| {
                    Control::Error(ShellError::new(
                        ErrorKind::Format,
                        span,
                        "delimited file is not valid UTF-8",
                        alloc,
                    ))
                })?;
                let sep = if sniff.label == "TSV" { '\t' } else { ',' };
                return Ok(Value::Table(parse_csv(text, false, sep, alloc)));
            }
            _ => {
                let _ = &ext;
            }
        }
    }

    match core::str::from_utf8(bytes.as_slice()) {
        Ok(text) => Ok(Value::string(text, alloc)),
        Err(_) => Ok(Value::binary(bytes.as_slice(), alloc)),
    }
}

fn builtin_save<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let append = args.has_flag("append", "a");
    let raw_mode = args.has_flag("raw", "r");
    let Some((v, s)) = args.pos.first() else {
        return Err(arity_err("save", "missing file path", span, alloc));
    };
    let raw_path = require_string(v, "path", *s, alloc).map_err(Control::Error)?;
    let cwd = eval.state().cwd.share();
    let home = eval.state().get_env("HOME").map(|h| AString::from_str_in(h, alloc));
    let path = resolve_path(cwd.as_ref(), raw_path.as_str(), home.as_ref().map(|h| h.as_str()), alloc);
    let ext = ext_lowercase(path.as_str(), alloc);

    let bytes = match &input {
        Value::Binary(b) => {
            let mut out = AVec::with_capacity_in(b.len(), alloc);
            out.extend_from_slice(b.as_ref());
            out
        }
        _ => {
            let text = if !raw_mode {
                match ext.as_ref().map(|e| e.as_str()) {
                    Some("json") => to_json_string(&input, false, span, alloc).map_err(Control::Error)?,
                    Some("toml") => to_toml_string(&input, span, alloc).map_err(Control::Error)?,
                    Some("csv") => to_csv_string(&input, span, alloc).map_err(Control::Error)?,
                    _ => to_text_string(&input, alloc),
                }
            } else {
                to_text_string(&input, alloc)
            };
            let mut out = AVec::with_capacity_in(text.len(), alloc);
            out.extend_from_slice(text.as_bytes());
            out
        }
    };

    let ans = eval.request(Request::WriteFile { path, bytes, append }, span)?;
    match ans {
        Answer::Ok => Ok(Value::Nothing),
        _ => Err(Control::Error(ShellError::new(
            ErrorKind::Io,
            span,
            "unexpected host answer for `save`",
            alloc,
        ))),
    }
}

fn resolve_arg_path<'a>(
    eval: &Evaluator<'a, '_>,
    val: &Value<'a>,
    span: Span,
) -> Result<AString<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let raw = require_string(val, "path", span, alloc).map_err(Control::Error)?;
    let cwd = eval.state().cwd.as_ref();
    let home = eval.state().get_env("HOME");
    Ok(resolve_path(cwd, raw.as_str(), home, alloc))
}

fn builtin_cp<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.len() < 2 {
        return Err(arity_err("cp", "expected <src> <dst>", span, alloc));
    }
    let recursive = args.has_flag("recursive", "r");
    let src = resolve_arg_path(eval, &args.pos[0].0, args.pos[0].1)?;
    let dst = resolve_arg_path(eval, &args.pos[1].0, args.pos[1].1)?;
    let _ = eval.request(Request::FsOp(FsOp::Cp { src, dst, recursive }), span)?;
    Ok(Value::Nothing)
}

fn builtin_mv<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.len() < 2 {
        return Err(arity_err("mv", "expected <src> <dst>", span, alloc));
    }
    let src = resolve_arg_path(eval, &args.pos[0].0, args.pos[0].1)?;
    let dst = resolve_arg_path(eval, &args.pos[1].0, args.pos[1].1)?;
    let _ = eval.request(Request::FsOp(FsOp::Mv { src, dst }), span)?;
    Ok(Value::Nothing)
}

fn builtin_rm<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.is_empty() {
        return Err(arity_err("rm", "expected at least one path", span, alloc));
    }
    let recursive = args.has_flag("recursive", "r");
    let force = args.has_flag("force", "f");
    for (v, s) in args.pos.iter() {
        let path = resolve_arg_path(eval, v, *s)?;
        let _ = eval.request(Request::FsOp(FsOp::Rm { path, recursive, force }), *s)?;
    }
    Ok(Value::Nothing)
}

fn builtin_mkdir<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.is_empty() {
        return Err(arity_err("mkdir", "expected at least one directory path", span, alloc));
    }
    let parents = args.has_flag("parents", "p");
    for (v, s) in args.pos.iter() {
        let path = resolve_arg_path(eval, v, *s)?;
        let _ = eval.request(Request::FsOp(FsOp::Mkdir { path, parents }), *s)?;
    }
    Ok(Value::Nothing)
}

fn builtin_touch<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.is_empty() {
        return Err(arity_err("touch", "expected at least one file path", span, alloc));
    }
    for (v, s) in args.pos.iter() {
        let path = resolve_arg_path(eval, v, *s)?;
        let _ = eval.request(Request::FsOp(FsOp::Touch { path }), *s)?;
    }
    Ok(Value::Nothing)
}

fn builtin_which<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.is_empty() {
        return Err(arity_err("which", "expected command name", span, alloc));
    }
    let mut table = Table::from_col_names(&["command", "path", "type"], alloc);
    for (v, s) in args.pos.iter() {
        let name = require_string(v, "command", *s, alloc).map_err(Control::Error)?;
        if let Some(exp) = eval.state().aliases.get(name.as_str()) {
            let row = [
                Value::string(name.as_str(), alloc),
                Value::String(exp.share()),
                Value::string("alias", alloc),
            ];
            table.push_row_values(&row);
        } else if eval.state().defs.contains_key(name.as_str()) {
            let row = [
                Value::string(name.as_str(), alloc),
                Value::string("custom", alloc),
                Value::string("custom", alloc),
            ];
            table.push_row_values(&row);
        } else if is_builtin(name.as_str()) {
            let row = [
                Value::string(name.as_str(), alloc),
                Value::string("built-in", alloc),
                Value::string("built-in", alloc),
            ];
            table.push_row_values(&row);
        } else {
            let ans = eval.request(Request::Which { name: AString::from_str_in(name.as_str(), alloc) }, *s)?;
            if let Answer::FoundPath(Some(found)) = ans {
                let row = [
                    Value::string(name.as_str(), alloc),
                    Value::string(found.as_str(), alloc),
                    Value::string("external", alloc),
                ];
                table.push_row_values(&row);
            }
        }
    }
    Ok(Value::Table(table))
}

fn builtin_ps<'a>(eval: &mut Evaluator<'a, '_>, span: Span) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let ans = eval.request(Request::ProcessList, span)?;
    let Answer::Processes(procs) = ans else {
        return Err(Control::Error(ShellError::new(
            ErrorKind::Io,
            span,
            "unexpected host answer for `ps`",
            alloc,
        )));
    };
    let mut table = Table::from_col_names(&["pid", "ppid", "name", "status", "cpu", "mem"], alloc);
    table.set_col_display("name", crate::value::ColDisplay::Process);
    table.set_col_display("status", crate::value::ColDisplay::Status);
    table.set_col_display("cpu", crate::value::ColDisplay::Percent);
    table.set_col_display("mem", crate::value::ColDisplay::FileSize);
    for p in procs.iter() {
        eval.charge(1, span)?;
        let row = [
            Value::Int(i64::from(p.pid)),
            Value::Int(i64::from(p.ppid)),
            Value::string(p.name.as_str(), alloc),
            Value::string(p.status.as_str(), alloc),
            Value::Float(p.cpu_pct),
            Value::Size(p.mem),
        ];
        table.push_row_values(&row);
    }
    Ok(Value::Table(table))
}

fn builtin_sys<'a>(eval: &mut Evaluator<'a, '_>, span: Span) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let ans = eval.request(Request::SysInfo, span)?;
    let Answer::Sys(s) = ans else {
        return Err(Control::Error(ShellError::new(
            ErrorKind::Io,
            span,
            "unexpected host answer for `sys`",
            alloc,
        )));
    };
    let mut rec = Record::new(alloc);
    rec.insert_str("host", Value::string(s.host.as_str(), alloc));
    rec.insert_str("os", Value::string(s.os.as_str(), alloc));
    rec.insert_str("arch", Value::string(s.arch.as_str(), alloc));
    rec.insert_str("cpus", Value::Int(i64::from(s.cpu_count)));
    rec.insert_str("mem_total", Value::Size(s.mem_total));
    rec.insert_str("mem_used", Value::Size(s.mem_used));
    rec.insert_str("uptime", Value::Duration(Nanos(s.uptime_ns)));
    Ok(Value::Record(rec))
}

// =============================================================================
// 2. Table & List Builtins
// =============================================================================

fn parse_binop_word(s: &str) -> Option<BinaryOp> {
    match s {
        "==" => Some(BinaryOp::Eq),
        "!=" => Some(BinaryOp::Ne),
        "<" => Some(BinaryOp::Lt),
        "<=" => Some(BinaryOp::Le),
        ">" => Some(BinaryOp::Gt),
        ">=" => Some(BinaryOp::Ge),
        "=~" => Some(BinaryOp::RegexMatch),
        "!~" => Some(BinaryOp::RegexNotMatch),
        "in" => Some(BinaryOp::In),
        "not-in" => Some(BinaryOp::NotIn),
        "starts-with" => Some(BinaryOp::StartsWith),
        "ends-with" => Some(BinaryOp::EndsWith),
        _ => None,
    }
}

enum WherePred<'a, 'e> {
    Closure(ARc<'a, ClosureData<'a>>),
    Expr(&'e Expr<'a>),
    Triple { lhs: &'e Expr<'a>, op: BinaryOp, rhs: Value<'a>, op_span: Span },
}

fn eval_where_pred<'a, 'm>(
    eval: &mut Evaluator<'a, 'm>,
    pred: &WherePred<'a, '_>,
    item: Value<'a>,
    span: Span,
) -> Result<bool, Control<'a>> {
    match pred {
        WherePred::Closure(c) => {
            let res = eval.invoke_closure(c, core::slice::from_ref(&item), item.share(), span)?;
            Ok(res.is_truthy())
        }
        WherePred::Expr(e) => {
            let res = eval.eval_expr_with_row(e, item)?;
            Ok(res.is_truthy())
        }
        WherePred::Triple { lhs, op, rhs, op_span } => {
            let lval = eval.eval_expr_with_row(lhs, item)?;
            let res = eval.eval_binary_val(*op, lval, rhs.share(), *op_span)?;
            Ok(res.is_truthy())
        }
    }
}

fn builtin_where<'a>(
    eval: &mut Evaluator<'a, '_>,
    call: &CallExpr<'a>,
    input: Value<'a>,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let span = call.head_span;
    let mut pos_exprs: AVec<'a, &Expr<'a>> = AVec::new_in(alloc);
    for a in call.args.iter() {
        if let CallArg::Positional(e) = a {
            pos_exprs.push(e);
        }
    }
    if pos_exprs.is_empty() {
        return Err(arity_err("where", "expected predicate expression or closure", span, alloc));
    }

    let pred = if pos_exprs.len() == 1 {
        let e = pos_exprs[0];
        if matches!(e.kind, ExprKind::Closure { .. } | ExprKind::Var(_)) {
            let v = eval.eval_expr(e, Value::Nothing)?;
            if let Value::Closure(c) = v {
                WherePred::Closure(c)
            } else {
                WherePred::Expr(e)
            }
        } else {
            WherePred::Expr(e)
        }
    } else if pos_exprs.len() == 3 {
        let op_val = eval.eval_expr(pos_exprs[1], Value::Nothing)?;
        let op_word = value_as_word(&op_val, alloc)
            .ok_or_else(|| type_err("expected comparison operator in `where`", pos_exprs[1].span, alloc))?;
        let op = parse_binop_word(op_word.as_str())
            .ok_or_else(|| type_err("unknown comparison operator in `where`", pos_exprs[1].span, alloc))?;
        let rhs = eval.eval_expr(pos_exprs[2], Value::Nothing)?;
        WherePred::Triple { lhs: pos_exprs[0], op, rhs, op_span: pos_exprs[1].span }
    } else {
        return Err(arity_err("where", "expected a single predicate expression or closure", span, alloc));
    };

    match input {
        Value::Table(t) => {
            let mut out = t.empty_like();
            for r in 0..t.nrows() {
                eval.charge(1, span)?;
                let rec = t.row_record(r).unwrap();
                if eval_where_pred(eval, &pred, Value::Record(rec.share()), span)? {
                    out.push_record(&rec);
                }
            }
            Ok(Value::Table(out))
        }
        Value::List(items) => {
            let mut out = PVec::new(alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                let item = items.get(i).unwrap().share();
                if eval_where_pred(eval, &pred, item.share(), span)? {
                    out.push(item);
                }
            }
            Ok(Value::List(out))
        }
        Value::Range(range) => {
            let items = range.collect_bounded(65_536, alloc).map_err(Control::Error)?;
            let mut out = PVec::new(alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                let item = items.get(i).unwrap().share();
                if eval_where_pred(eval, &pred, item.share(), span)? {
                    out.push(item);
                }
            }
            Ok(Value::List(out))
        }
        other => {
            let mut msg = AString::from_str_in("`where` expects a table, list, or range, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_select<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.is_empty() {
        return Err(arity_err("select", "expected at least one column or index", span, alloc));
    }
    let all_ints = args.pos.iter().all(|(v, _)| matches!(v, Value::Int(_)));
    if all_ints {
        return match input {
            Value::Table(t) => {
                let mut out = t.empty_like();
                for (v, s) in args.pos.iter() {
                    let idx = require_int(v, "index", *s, alloc).map_err(Control::Error)?;
                    if idx < 0 || (idx as usize) >= t.nrows() {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::IndexOutOfBounds,
                            *s,
                            "row index out of bounds",
                            alloc,
                        )));
                    }
                    let rec = t.row_record(idx as usize).unwrap();
                    out.push_record(&rec);
                }
                Ok(Value::Table(out))
            }
            Value::List(items) => {
                let mut out = PVec::new(alloc);
                for (v, s) in args.pos.iter() {
                    let idx = require_int(v, "index", *s, alloc).map_err(Control::Error)?;
                    if idx < 0 || (idx as usize) >= items.len() {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::IndexOutOfBounds,
                            *s,
                            "list index out of bounds",
                            alloc,
                        )));
                    }
                    out.push(items.get(idx as usize).unwrap().share());
                }
                Ok(Value::List(out))
            }
            other => {
                let mut msg = AString::from_str_in("cannot select row indices from ", alloc);
                msg.push_str(other.type_name());
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        };
    }

    let mut names = AVec::with_capacity_in(args.pos.len(), alloc);
    for (v, s) in args.pos.iter() {
        names.push(value_as_col_name(v, *s, alloc).map_err(Control::Error)?);
    }
    let mut name_refs = AVec::with_capacity_in(names.len(), alloc);
    for n in names.iter() {
        name_refs.push(n.as_str());
    }

    match input {
        Value::Table(t) => t.select_columns(name_refs.as_slice()).map(Value::Table).map_err(|mut e| {
            e.span = span;
            Control::Error(e)
        }),
        Value::Record(rec) => {
            let mut out = Record::new(alloc);
            for &n in name_refs.iter() {
                match rec.get(n) {
                    Some(v) => out.insert_str(n, v.share()),
                    None => {
                        let mut msg = AString::from_str_in("record has no field `", alloc);
                        msg.push_str(n);
                        msg.push('`');
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::MissingColumn,
                            span,
                            msg.as_str(),
                            alloc,
                        )));
                    }
                }
            }
            Ok(Value::Record(out))
        }
        Value::List(items) => {
            let mut table = Table::from_col_names(name_refs.as_slice(), alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                let Value::Record(rec) = items.get(i).unwrap() else {
                    return Err(type_err("`select` on a list requires records", span, alloc));
                };
                let mut row = AVec::with_capacity_in(name_refs.len(), alloc);
                for &n in name_refs.iter() {
                    let Some(v) = rec.get(n) else {
                        let mut msg = AString::from_str_in("record has no field `", alloc);
                        msg.push_str(n);
                        msg.push('`');
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::MissingColumn,
                            span,
                            msg.as_str(),
                            alloc,
                        )));
                    };
                    row.push(v.share());
                }
                table.push_row_values(row.as_slice());
            }
            Ok(Value::Table(table))
        }
        other => {
            let mut msg = AString::from_str_in("`select` expects a table, record, or list, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_reject<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.is_empty() {
        return Err(arity_err("reject", "expected at least one column or index", span, alloc));
    }
    let all_ints = args.pos.iter().all(|(v, _)| matches!(v, Value::Int(_)));
    if all_ints {
        let mut drops = AVec::with_capacity_in(args.pos.len(), alloc);
        for (v, s) in args.pos.iter() {
            drops.push(require_int(v, "index", *s, alloc).map_err(Control::Error)?);
        }
        return match input {
            Value::Table(t) => {
                let mut out = t.empty_like();
                for r in 0..t.nrows() {
                    eval.charge(1, span)?;
                    if !drops.iter().any(|&d| d >= 0 && d as usize == r) {
                        out.push_record(&t.row_record(r).unwrap());
                    }
                }
                Ok(Value::Table(out))
            }
            Value::List(items) => {
                let mut out = PVec::new(alloc);
                for i in 0..items.len() {
                    eval.charge(1, span)?;
                    if !drops.iter().any(|&d| d >= 0 && d as usize == i) {
                        out.push(items.get(i).unwrap().share());
                    }
                }
                Ok(Value::List(out))
            }
            other => {
                let mut msg = AString::from_str_in("cannot reject row indices from ", alloc);
                msg.push_str(other.type_name());
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        };
    }

    let mut names = AVec::with_capacity_in(args.pos.len(), alloc);
    for (v, s) in args.pos.iter() {
        names.push(value_as_col_name(v, *s, alloc).map_err(Control::Error)?);
    }
    let mut name_refs = AVec::with_capacity_in(names.len(), alloc);
    for n in names.iter() {
        name_refs.push(n.as_str());
    }

    match input {
        Value::Table(t) => Ok(Value::Table(t.reject_columns(name_refs.as_slice()))),
        Value::Record(mut rec) => {
            for &n in name_refs.iter() {
                rec = rec.without(n);
            }
            Ok(Value::Record(rec))
        }
        Value::List(items) => {
            let mut out = PVec::new(alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                match items.get(i).unwrap() {
                    Value::Record(r) => {
                        let mut nr = r.share();
                        for &n in name_refs.iter() {
                            nr = nr.without(n);
                        }
                        out.push(Value::Record(nr));
                    }
                    other => out.push(other.share()),
                }
            }
            Ok(Value::List(out))
        }
        other => {
            let mut msg = AString::from_str_in("`reject` expects a table, record, or list, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_get<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let optional = args.has_flag("ignore-errors", "i") || args.has_flag("optional", "o");
    if args.pos.is_empty() {
        return Err(arity_err("get", "expected a field name, index, or cell path", span, alloc));
    }
    let mut cur = input;
    for (v, s) in args.pos.iter() {
        match v {
            Value::String(name) => {
                let raw = name.as_ref();
                let direct_hit = match &cur {
                    Value::Record(r) => r.contains_key(raw),
                    Value::Table(t) => t.col_index(raw).is_some(),
                    _ => false,
                };
                if direct_hit || !raw.contains('.') {
                    let member = PathMember::Field { name: name.share(), optional };
                    cur = cur.follow_member(&member, *s, alloc).map_err(Control::Error)?;
                } else {
                    for seg in raw.split('.') {
                        if seg.is_empty() {
                            continue;
                        }
                        let (clean, seg_opt) = match seg.strip_suffix('?') {
                            Some(s) => (s, true),
                            None => (seg, false),
                        };
                        let opt = optional || seg_opt;
                        let member = if let Ok(idx) = clean.parse::<usize>() {
                            PathMember::Index { index: idx, optional: opt }
                        } else {
                            PathMember::Field { name: Rc::from_str_in(clean, alloc), optional: opt }
                        };
                        cur = cur.follow_member(&member, *s, alloc).map_err(Control::Error)?;
                    }
                }
            }
            Value::Int(idx) => {
                if *idx < 0 {
                    if optional {
                        cur = Value::Nothing;
                    } else {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::IndexOutOfBounds,
                            *s,
                            "negative index in `get`",
                            alloc,
                        )));
                    }
                } else {
                    let member = PathMember::Index { index: *idx as usize, optional };
                    cur = cur.follow_member(&member, *s, alloc).map_err(Control::Error)?;
                }
            }
            Value::CellPath(path) => {
                for m in path.iter() {
                    let m = match m {
                        PathMember::Field { name, optional: mo } => {
                            PathMember::Field { name: name.share(), optional: *mo || optional }
                        }
                        PathMember::Index { index, optional: mo } => {
                            PathMember::Index { index: *index, optional: *mo || optional }
                        }
                    };
                    cur = cur.follow_member(&m, *s, alloc).map_err(Control::Error)?;
                }
            }
            other => {
                let mut msg = AString::from_str_in("`get` expects a field, index, or cell-path, got ", alloc);
                msg.push_str(other.type_name());
                return Err(Control::Error(ShellError::new(ErrorKind::Type, *s, msg.as_str(), alloc)));
            }
        }
    }
    Ok(cur)
}

fn compare_values_for_sort<'a>(a: &Value<'a>, b: &Value<'a>, ignore_case: bool, alloc: Alloc<'a>) -> Ordering {
    if ignore_case {
        if let (Value::String(sa), Value::String(sb)) = (a, b) {
            let la = str_to_lowercase(sa.as_ref(), alloc);
            let lb = str_to_lowercase(sb.as_ref(), alloc);
            return la.as_str().cmp(lb.as_str());
        }
    }
    match (a, b) {
        (Value::Float(fa), Value::Float(fb)) => fa.total_cmp(fb),
        (Value::Int(ia), Value::Float(fb)) => (*ia as f64).total_cmp(fb),
        (Value::Float(fa), Value::Int(ib)) => fa.total_cmp(&(*ib as f64)),
        _ => a.compare(b).unwrap_or_else(|| a.type_name().cmp(b.type_name())),
    }
}

fn builtin_sort_by<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let reverse = args.has_flag("reverse", "r");
    let ignore_case = args.has_flag("ignore-case", "i");

    match input {
        Value::Table(t) => {
            if args.pos.is_empty() {
                return Err(arity_err("sort-by", "expected column name or closure for table", span, alloc));
            }
            let n = t.nrows();
            let mut keys: AVec<'a, (usize, AVec<'a, Value<'a>>)> = AVec::with_capacity_in(n, alloc);
            if let Value::Closure(c) = &args.pos[0].0 {
                let c = c.share();
                for r in 0..n {
                    eval.charge(1, span)?;
                    let rec = Value::Record(t.row_record(r).unwrap());
                    let k = eval.invoke_closure(&c, core::slice::from_ref(&rec), rec.share(), span)?;
                    let mut kv = AVec::with_capacity_in(1, alloc);
                    kv.push(k);
                    keys.push((r, kv));
                }
            } else {
                let mut col_idxs = AVec::with_capacity_in(args.pos.len(), alloc);
                for (v, s) in args.pos.iter() {
                    let cname = value_as_col_name(v, *s, alloc).map_err(Control::Error)?;
                    let Some(ci) = t.col_index(cname.as_str()) else {
                        let mut msg = AString::from_str_in("table has no column `", alloc);
                        msg.push_str(cname.as_str());
                        msg.push('`');
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::MissingColumn,
                            *s,
                            msg.as_str(),
                            alloc,
                        )));
                    };
                    col_idxs.push(ci);
                }
                for r in 0..n {
                    eval.charge(1, span)?;
                    let mut kv = AVec::with_capacity_in(col_idxs.len(), alloc);
                    for &ci in col_idxs.iter() {
                        kv.push(t.cell(r, ci).unwrap().share());
                    }
                    keys.push((r, kv));
                }
            }

            keys.sort_unstable_by(|(ia, ka), (ib, kb)| {
                for (va, vb) in ka.iter().zip(kb.iter()) {
                    let ord = compare_values_for_sort(va, vb, ignore_case, alloc);
                    if ord != Ordering::Equal {
                        return if reverse { ord.reverse() } else { ord };
                    }
                }
                ia.cmp(ib)
            });

            let mut out = t.empty_like();
            for (r, _) in keys.iter() {
                eval.charge(1, span)?;
                out.push_record(&t.row_record(*r).unwrap());
            }
            Ok(Value::Table(out))
        }
        Value::List(items) => {
            let n = items.len();
            let mut keyed: AVec<'a, (usize, Value<'a>)> = AVec::with_capacity_in(n, alloc);
            if args.pos.is_empty() {
                for i in 0..n {
                    eval.charge(1, span)?;
                    keyed.push((i, items.get(i).unwrap().share()));
                }
            } else if let Value::Closure(c) = &args.pos[0].0 {
                let c = c.share();
                for i in 0..n {
                    eval.charge(1, span)?;
                    let item = items.get(i).unwrap().share();
                    let k = eval.invoke_closure(&c, core::slice::from_ref(&item), item.share(), span)?;
                    keyed.push((i, k));
                }
            } else {
                let cname = value_as_col_name(&args.pos[0].0, args.pos[0].1, alloc).map_err(Control::Error)?;
                let member = PathMember::Field {
                    name: Rc::from_str_in(cname.as_str(), alloc),
                    optional: false,
                };
                for i in 0..n {
                    eval.charge(1, span)?;
                    let k = items.get(i).unwrap().follow_member(&member, span, alloc).map_err(Control::Error)?;
                    keyed.push((i, k));
                }
            }

            keyed.sort_unstable_by(|(ia, ka), (ib, kb)| {
                let ord = compare_values_for_sort(ka, kb, ignore_case, alloc);
                if ord != Ordering::Equal {
                    if reverse { ord.reverse() } else { ord }
                } else {
                    ia.cmp(ib)
                }
            });

            let mut out = PVec::new(alloc);
            for (i, _) in keyed.iter() {
                out.push(items.get(*i).unwrap().share());
            }
            Ok(Value::List(out))
        }
        other => {
            let mut msg = AString::from_str_in("`sort-by` expects a table or list, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_reverse<'a>(
    eval: &mut Evaluator<'a, '_>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::Table(t) => {
            let mut out = t.empty_like();
            for r in (0..t.nrows()).rev() {
                eval.charge(1, span)?;
                out.push_record(&t.row_record(r).unwrap());
            }
            Ok(Value::Table(out))
        }
        Value::List(items) => {
            let mut out = PVec::new(alloc);
            for i in (0..items.len()).rev() {
                eval.charge(1, span)?;
                out.push(items.get(i).unwrap().share());
            }
            Ok(Value::List(out))
        }
        Value::String(s) => {
            let mut out = AString::with_capacity_in(s.len(), alloc);
            for ch in s.as_ref().chars().rev() {
                out.push(ch);
            }
            Ok(Value::string(out.as_str(), alloc))
        }
        other => {
            let mut msg = AString::from_str_in("`reverse` expects a table, list, or string, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn take_range_n<'a>(
    eval: &mut Evaluator<'a, '_>,
    range: RangeVal,
    n: usize,
    span: Span,
) -> Result<PVec<'a, Value<'a>>, Control<'a>> {
    let alloc = eval.alloc();
    if range.step == 0 {
        return Err(Control::Error(ShellError::new(
            ErrorKind::Type,
            span,
            "range step cannot be 0",
            alloc,
        )));
    }
    let mut out = PVec::new(alloc);
    let mut cur = range.start;
    for _ in 0..n {
        if let Some(end) = range.end {
            let in_bounds = if range.step > 0 {
                if range.inclusive { cur <= end } else { cur < end }
            } else if range.inclusive {
                cur >= end
            } else {
                cur > end
            };
            if !in_bounds {
                break;
            }
        }
        eval.charge(1, span)?;
        out.push(Value::Int(cur));
        match cur.checked_add(range.step) {
            Some(next) => cur = next,
            None => break,
        }
    }
    Ok(out)
}

fn builtin_first<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if let Some((v, s)) = args.pos.first() {
        let n = require_int(v, "count", *s, alloc).map_err(Control::Error)?;
        let n = n.max(0) as usize;
        return take_n(eval, input, n, span);
    }
    match input {
        Value::Table(t) => match t.row_record(0) {
            Some(r) => Ok(Value::Record(r)),
            None => Err(Control::Error(ShellError::new(
                ErrorKind::IndexOutOfBounds,
                span,
                "cannot take `first` of empty table",
                alloc,
            ))),
        },
        Value::List(items) => match items.first() {
            Some(v) => Ok(v.share()),
            None => Err(Control::Error(ShellError::new(
                ErrorKind::IndexOutOfBounds,
                span,
                "cannot take `first` of empty list",
                alloc,
            ))),
        },
        Value::Range(r) => {
            let items = take_range_n(eval, r, 1, span)?;
            match items.first() {
                Some(v) => Ok(v.share()),
                None => Err(Control::Error(ShellError::new(
                    ErrorKind::IndexOutOfBounds,
                    span,
                    "cannot take `first` of empty range",
                    alloc,
                ))),
            }
        }
        Value::Binary(b) => match b.first() {
            Some(&byte) => Ok(Value::Int(i64::from(byte))),
            None => Err(Control::Error(ShellError::new(
                ErrorKind::IndexOutOfBounds,
                span,
                "cannot take `first` of empty binary",
                alloc,
            ))),
        },
        other => {
            let mut msg = AString::from_str_in("`first` expects a table, list, or range, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn take_n<'a>(
    eval: &mut Evaluator<'a, '_>,
    input: Value<'a>,
    n: usize,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::Table(t) => {
            let count = n.min(t.nrows());
            let mut out = t.empty_like();
            for r in 0..count {
                eval.charge(1, span)?;
                out.push_record(&t.row_record(r).unwrap());
            }
            Ok(Value::Table(out))
        }
        Value::List(items) => {
            let count = n.min(items.len());
            let mut out = PVec::new(alloc);
            for i in 0..count {
                eval.charge(1, span)?;
                out.push(items.get(i).unwrap().share());
            }
            Ok(Value::List(out))
        }
        Value::Range(r) => Ok(Value::List(take_range_n(eval, r, n, span)?)),
        Value::Binary(b) => {
            let count = n.min(b.len());
            Ok(Value::binary(&b.as_ref()[..count], alloc))
        }
        Value::String(s) => {
            let mut out = AString::new_in(alloc);
            for ch in s.as_ref().chars().take(n) {
                out.push(ch);
            }
            Ok(Value::string(out.as_str(), alloc))
        }
        other => {
            let mut msg = AString::from_str_in("`take` expects a table, list, or range, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_last<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if let Some((v, s)) = args.pos.first() {
        let n = require_int(v, "count", *s, alloc).map_err(Control::Error)?.max(0) as usize;
        return match input {
            Value::Table(t) => {
                let start = t.nrows().saturating_sub(n);
                let mut out = t.empty_like();
                for r in start..t.nrows() {
                    eval.charge(1, span)?;
                    out.push_record(&t.row_record(r).unwrap());
                }
                Ok(Value::Table(out))
            }
            Value::List(items) => {
                let start = items.len().saturating_sub(n);
                let mut out = PVec::new(alloc);
                for i in start..items.len() {
                    eval.charge(1, span)?;
                    out.push(items.get(i).unwrap().share());
                }
                Ok(Value::List(out))
            }
            other => {
                let mut msg = AString::from_str_in("`last` expects a table or list, got ", alloc);
                msg.push_str(other.type_name());
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        };
    }
    match input {
        Value::Table(t) => {
            if t.nrows() == 0 {
                return Err(Control::Error(ShellError::new(
                    ErrorKind::IndexOutOfBounds,
                    span,
                    "cannot take `last` of empty table",
                    alloc,
                )));
            }
            Ok(Value::Record(t.row_record(t.nrows() - 1).unwrap()))
        }
        Value::List(items) => match items.last() {
            Some(v) => Ok(v.share()),
            None => Err(Control::Error(ShellError::new(
                ErrorKind::IndexOutOfBounds,
                span,
                "cannot take `last` of empty list",
                alloc,
            ))),
        },
        other => {
            let mut msg = AString::from_str_in("`last` expects a table or list, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_skip<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((v, s)) = args.pos.first() else {
        return Err(arity_err("skip", "expected count", span, alloc));
    };
    let n = require_int(v, "count", *s, alloc).map_err(Control::Error)?.max(0) as usize;
    match input {
        Value::Table(t) => {
            let mut out = t.empty_like();
            for r in n.min(t.nrows())..t.nrows() {
                eval.charge(1, span)?;
                out.push_record(&t.row_record(r).unwrap());
            }
            Ok(Value::Table(out))
        }
        Value::List(items) => {
            let mut out = PVec::new(alloc);
            for i in n.min(items.len())..items.len() {
                eval.charge(1, span)?;
                out.push(items.get(i).unwrap().share());
            }
            Ok(Value::List(out))
        }
        Value::Range(mut r) => {
            let offset = (n as i64).saturating_mul(r.step);
            r.start = r.start.saturating_add(offset);
            Ok(Value::Range(r))
        }
        other => {
            let mut msg = AString::from_str_in("`skip` expects a table, list, or range, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_take<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((v, s)) = args.pos.first() else {
        return Err(arity_err("take", "expected count", span, alloc));
    };
    let n = require_int(v, "count", *s, alloc).map_err(Control::Error)?.max(0) as usize;
    take_n(eval, input, n, span)
}

fn builtin_uniq<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let with_count = args.has_flag("count", "c");
    match input {
        Value::List(items) => {
            let mut seen: AVec<'a, (Value<'a>, i64)> = AVec::new_in(alloc);
            for i in 0..items.len() {
                let item = items.get(i).unwrap();
                let mut found = false;
                for (sv, cnt) in seen.iter_mut() {
                    eval.charge(1, span)?;
                    if *sv == *item {
                        *cnt += 1;
                        found = true;
                        break;
                    }
                }
                if !found {
                    eval.charge(1, span)?;
                    seen.push((item.share(), 1));
                }
            }
            if with_count {
                let mut table = Table::from_col_names(&["value", "count"], alloc);
                for (sv, cnt) in seen.iter() {
                    table.push_row_values(&[sv.share(), Value::Int(*cnt)]);
                }
                Ok(Value::Table(table))
            } else {
                let mut out = PVec::new(alloc);
                for (sv, _) in seen.iter() {
                    out.push(sv.share());
                }
                Ok(Value::List(out))
            }
        }
        Value::Table(t) => {
            let mut seen: AVec<'a, (Record<'a>, i64)> = AVec::new_in(alloc);
            for r in 0..t.nrows() {
                let rec = t.row_record(r).unwrap();
                let mut found = false;
                for (sr, cnt) in seen.iter_mut() {
                    eval.charge(1, span)?;
                    if *sr == rec {
                        *cnt += 1;
                        found = true;
                        break;
                    }
                }
                if !found {
                    eval.charge(1, span)?;
                    seen.push((rec, 1));
                }
            }
            if with_count {
                let mut table = Table::from_col_names(&["value", "count"], alloc);
                for (sr, cnt) in seen.iter() {
                    table.push_row_values(&[Value::Record(sr.share()), Value::Int(*cnt)]);
                }
                Ok(Value::Table(table))
            } else {
                let mut out = t.empty_like();
                for (sr, _) in seen.iter() {
                    out.push_record(sr);
                }
                Ok(Value::Table(out))
            }
        }
        other => {
            let mut msg = AString::from_str_in("`uniq` expects a list or table, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_length<'a>(
    eval: &Evaluator<'a, '_>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::Table(t) => Ok(Value::Int(t.nrows() as i64)),
        Value::List(l) => Ok(Value::Int(l.len() as i64)),
        Value::Record(r) => Ok(Value::Int(r.len() as i64)),
        Value::String(s) => Ok(Value::Int(s.as_ref().chars().count() as i64)),
        Value::Binary(b) => Ok(Value::Int(b.len() as i64)),
        Value::Range(r) => {
            let Some(end) = r.end else {
                return Err(Control::Error(ShellError::new(
                    ErrorKind::Budget,
                    span,
                    "cannot take `length` of unbounded range",
                    alloc,
                )));
            };
            if r.step == 0 {
                return Err(Control::Error(ShellError::new(
                    ErrorKind::Type,
                    span,
                    "range step cannot be 0",
                    alloc,
                )));
            }
            let count = if r.step > 0 {
                if (r.inclusive && r.start > end) || (!r.inclusive && r.start >= end) {
                    0
                } else {
                    let span_len = (end as i128) - (r.start as i128) + if r.inclusive { 0 } else { -1 };
                    (span_len / (r.step as i128) + 1) as i64
                }
            } else if (r.inclusive && r.start < end) || (!r.inclusive && r.start <= end) {
                0
            } else {
                let span_len = (r.start as i128) - (end as i128) + if r.inclusive { 0 } else { -1 };
                (span_len / (-(r.step as i128)) + 1) as i64
            };
            Ok(Value::Int(count))
        }
        Value::Nothing => Ok(Value::Int(0)),
        other => {
            let mut msg = AString::from_str_in("cannot take `length` of ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_compact<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::List(items) => {
            let mut out = PVec::new(alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                let v = items.get(i).unwrap();
                if !matches!(v, Value::Nothing) {
                    out.push(v.share());
                }
            }
            Ok(Value::List(out))
        }
        Value::Table(t) => {
            let mut col_idxs = AVec::new_in(alloc);
            for (v, s) in args.pos.iter() {
                let cname = value_as_col_name(v, *s, alloc).map_err(Control::Error)?;
                let Some(ci) = t.col_index(cname.as_str()) else {
                    let mut msg = AString::from_str_in("table has no column `", alloc);
                    msg.push_str(cname.as_str());
                    msg.push('`');
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::MissingColumn,
                        *s,
                        msg.as_str(),
                        alloc,
                    )));
                };
                col_idxs.push(ci);
            }
            let mut out = t.empty_like();
            for r in 0..t.nrows() {
                eval.charge(1, span)?;
                let keep = if col_idxs.is_empty() {
                    (0..t.ncols()).any(|c| !matches!(t.cell(r, c), Some(Value::Nothing)))
                } else {
                    col_idxs.iter().all(|&c| !matches!(t.cell(r, c), Some(Value::Nothing)))
                };
                if keep {
                    out.push_record(&t.row_record(r).unwrap());
                }
            }
            Ok(Value::Table(out))
        }
        other => {
            let mut msg = AString::from_str_in("`compact` expects a list or table, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn flatten_list_into<'a>(
    eval: &mut Evaluator<'a, '_>,
    items: &PVec<'a, Value<'a>>,
    recursive: bool,
    out: &mut PVec<'a, Value<'a>>,
    span: Span,
) -> Result<(), Control<'a>> {
    for i in 0..items.len() {
        eval.charge(1, span)?;
        match items.get(i).unwrap() {
            Value::List(inner) => {
                if recursive {
                    flatten_list_into(eval, inner, true, out, span)?;
                } else {
                    for j in 0..inner.len() {
                        eval.charge(1, span)?;
                        out.push(inner.get(j).unwrap().share());
                    }
                }
            }
            Value::Table(t) => {
                for r in 0..t.nrows() {
                    eval.charge(1, span)?;
                    out.push(Value::Record(t.row_record(r).unwrap()));
                }
            }
            other => out.push(other.share()),
        }
    }
    Ok(())
}

fn builtin_flatten<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let all = args.has_flag("all", "a");
    match input {
        Value::List(items) => {
            let mut out = PVec::new(alloc);
            flatten_list_into(eval, &items, all, &mut out, span)?;
            if let Some(table) = Table::try_from_list(&out, alloc) {
                Ok(Value::Table(table))
            } else {
                Ok(Value::List(out))
            }
        }
        Value::Table(t) => {
            if args.pos.is_empty() {
                return Ok(Value::Table(t));
            }
            let mut cur = t;
            for (v, s) in args.pos.iter() {
                let cname = value_as_col_name(v, *s, alloc).map_err(Control::Error)?;
                let Some(ci) = cur.col_index(cname.as_str()) else {
                    let mut msg = AString::from_str_in("table has no column `", alloc);
                    msg.push_str(cname.as_str());
                    msg.push('`');
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::MissingColumn,
                        *s,
                        msg.as_str(),
                        alloc,
                    )));
                };
                let mut next = Table::new(cur.columns.share(), alloc);
                for r in 0..cur.nrows() {
                    eval.charge(1, span)?;
                    let rec = cur.row_record(r).unwrap();
                    match cur.cell(r, ci).unwrap() {
                        Value::List(inner) => {
                            for j in 0..inner.len() {
                                eval.charge(1, span)?;
                                let mut expanded = rec.share();
                                expanded.insert_str(cname.as_str(), inner.get(j).unwrap().share());
                                next.push_record(&expanded);
                            }
                        }
                        _ => next.push_record(&rec),
                    }
                }
                cur = next;
            }
            Ok(Value::Table(cur))
        }
        other => Ok(other),
    }
}

fn builtin_enumerate<'a>(
    eval: &mut Evaluator<'a, '_>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let mut table = Table::from_col_names(&["index", "item"], alloc);
    match input {
        Value::List(items) => {
            for i in 0..items.len() {
                eval.charge(1, span)?;
                table.push_row_values(&[Value::Int(i as i64), items.get(i).unwrap().share()]);
            }
            Ok(Value::Table(table))
        }
        Value::Table(t) => {
            for r in 0..t.nrows() {
                eval.charge(1, span)?;
                table.push_row_values(&[Value::Int(r as i64), Value::Record(t.row_record(r).unwrap())]);
            }
            Ok(Value::Table(table))
        }
        Value::Range(r) => {
            let items = r.collect_bounded(65_536, alloc).map_err(Control::Error)?;
            for i in 0..items.len() {
                eval.charge(1, span)?;
                table.push_row_values(&[Value::Int(i as i64), items.get(i).unwrap().share()]);
            }
            Ok(Value::Table(table))
        }
        other => {
            let mut msg = AString::from_str_in("`enumerate` expects a list, table, or range, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_transpose<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let mut custom_names = AVec::with_capacity_in(args.pos.len(), alloc);
    for (v, s) in args.pos.iter() {
        custom_names.push(value_as_col_name(v, *s, alloc).map_err(Control::Error)?);
    }

    match input {
        Value::Record(rec) => {
            let c0 = custom_names.first().map(|s| s.as_str()).unwrap_or("column0");
            let c1 = custom_names.get(1).map(|s| s.as_str()).unwrap_or("column1");
            let mut table = Table::from_col_names(&[c0, c1], alloc);
            for (k, v) in rec.iter() {
                eval.charge(1, span)?;
                table.push_row_values(&[Value::string(k, alloc), v.share()]);
            }
            Ok(Value::Table(table))
        }
        Value::Table(t) => {
            let total_cols = 1 + t.nrows();
            let mut headers = PVec::new(alloc);
            for i in 0..total_cols {
                if let Some(custom) = custom_names.get(i) {
                    headers.push(Rc::from_str_in(custom.as_str(), alloc));
                } else {
                    let mut cname = AString::new_in(alloc);
                    let _ = write!(cname, "column{i}");
                    headers.push(Rc::from_str_in(cname.as_str(), alloc));
                }
            }
            let mut out = Table::new(headers, alloc);
            for c in 0..t.ncols() {
                eval.charge(1, span)?;
                let mut row = AVec::with_capacity_in(total_cols, alloc);
                row.push(Value::String(t.columns.get(c).unwrap().share()));
                for r in 0..t.nrows() {
                    row.push(t.cell(r, c).unwrap().share());
                }
                out.push_row_values(row.as_slice());
            }
            Ok(Value::Table(out))
        }
        other => {
            let mut msg = AString::from_str_in("`transpose` expects a table or record, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_headers<'a>(
    eval: &mut Evaluator<'a, '_>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::Table(t) => {
            if t.nrows() == 0 {
                return Ok(Value::Table(t));
            }
            let mut headers = PVec::new(alloc);
            for c in 0..t.ncols() {
                let h = t.cell(0, c).unwrap().to_display_string(alloc);
                headers.push(Rc::from_str_in(h.as_str(), alloc));
            }
            let mut out = Table::new(headers, alloc);
            for r in 1..t.nrows() {
                eval.charge(1, span)?;
                let mut row = AVec::with_capacity_in(t.ncols(), alloc);
                for c in 0..t.ncols() {
                    row.push(t.cell(r, c).unwrap().share());
                }
                out.push_row_values(row.as_slice());
            }
            Ok(Value::Table(out))
        }
        Value::List(rows) => {
            if rows.is_empty() {
                return Ok(Value::Table(Table::new(PVec::new(alloc), alloc)));
            }
            let Value::List(first_row) = rows.get(0).unwrap() else {
                return Err(type_err("`headers` on a list expects a list of lists", span, alloc));
            };
            let mut headers = PVec::new(alloc);
            for c in 0..first_row.len() {
                let h = first_row.get(c).unwrap().to_display_string(alloc);
                headers.push(Rc::from_str_in(h.as_str(), alloc));
            }
            let mut out = Table::new(headers, alloc);
            for r in 1..rows.len() {
                eval.charge(1, span)?;
                let Value::List(cells) = rows.get(r).unwrap() else {
                    return Err(type_err("`headers` expects all rows to be lists", span, alloc));
                };
                let mut vals = AVec::with_capacity_in(cells.len(), alloc);
                for c in 0..cells.len() {
                    vals.push(cells.get(c).unwrap().share());
                }
                out.push_row_values(vals.as_slice());
            }
            Ok(Value::Table(out))
        }
        other => {
            let mut msg = AString::from_str_in("`headers` expects a table or list of lists, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_columns<'a>(
    eval: &Evaluator<'a, '_>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::Table(t) => {
            let mut out = PVec::new(alloc);
            for c in 0..t.ncols() {
                out.push(Value::String(t.columns.get(c).unwrap().share()));
            }
            Ok(Value::List(out))
        }
        Value::Record(r) => {
            let mut out = PVec::new(alloc);
            for i in 0..r.len() {
                out.push(Value::String(r.keys.get(i).unwrap().share()));
            }
            Ok(Value::List(out))
        }
        other => {
            let mut msg = AString::from_str_in("`columns` expects a table or record, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_values<'a>(
    eval: &Evaluator<'a, '_>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::Record(r) => Ok(Value::List(r.values.share())),
        Value::Table(t) => {
            let mut out = PVec::new(alloc);
            for c in 0..t.ncols() {
                out.push(Value::List(t.cols.get(c).unwrap().share()));
            }
            Ok(Value::List(out))
        }
        other => {
            let mut msg = AString::from_str_in("`values` expects a record or table, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_append<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((rhs, _)) = args.pos.first() else {
        return Err(arity_err("append", "expected value to append", span, alloc));
    };
    match (input, rhs) {
        (Value::List(mut items), Value::List(more)) => {
            for i in 0..more.len() {
                eval.charge(1, span)?;
                items.push(more.get(i).unwrap().share());
            }
            Ok(Value::List(items))
        }
        (Value::List(mut items), other) => {
            items.push(other.share());
            Ok(Value::List(items))
        }
        (Value::Table(mut t), Value::Record(r)) => {
            t.push_record(r);
            Ok(Value::Table(t))
        }
        (Value::Table(mut t), Value::Table(other)) => {
            for r in 0..other.nrows() {
                eval.charge(1, span)?;
                t.push_record(&other.row_record(r).unwrap());
            }
            Ok(Value::Table(t))
        }
        (lhs, rhs) => {
            let mut out = PVec::new(alloc);
            out.push(lhs);
            if let Value::List(more) = rhs {
                for i in 0..more.len() {
                    out.push(more.get(i).unwrap().share());
                }
            } else {
                out.push(rhs.share());
            }
            Ok(Value::List(out))
        }
    }
}

fn builtin_prepend<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((lhs, _)) = args.pos.first() else {
        return Err(arity_err("prepend", "expected value to prepend", span, alloc));
    };
    match (lhs, input) {
        (Value::List(front), Value::List(back)) => {
            let mut out = front.share();
            for i in 0..back.len() {
                eval.charge(1, span)?;
                out.push(back.get(i).unwrap().share());
            }
            Ok(Value::List(out))
        }
        (val, Value::List(back)) => {
            let mut out = PVec::new(alloc);
            out.push(val.share());
            for i in 0..back.len() {
                eval.charge(1, span)?;
                out.push(back.get(i).unwrap().share());
            }
            Ok(Value::List(out))
        }
        (Value::Record(r), Value::Table(back)) => {
            let mut out = Table::new(back.columns.share(), alloc);
            out.push_record(r);
            for i in 0..back.nrows() {
                eval.charge(1, span)?;
                out.push_record(&back.row_record(i).unwrap());
            }
            Ok(Value::Table(out))
        }
        (Value::Table(front), Value::Table(back)) => {
            let mut out = front.share();
            for i in 0..back.nrows() {
                eval.charge(1, span)?;
                out.push_record(&back.row_record(i).unwrap());
            }
            Ok(Value::Table(out))
        }
        (front, back) => {
            let mut out = PVec::new(alloc);
            out.push(front.share());
            out.push(back);
            Ok(Value::List(out))
        }
    }
}

fn iter_to_pvec<'a>(
    eval: &mut Evaluator<'a, '_>,
    v: Value<'a>,
    span: Span,
) -> Result<PVec<'a, Value<'a>>, Control<'a>> {
    let alloc = eval.alloc();
    match v {
        Value::List(l) => Ok(l),
        Value::Table(t) => {
            let mut out = PVec::new(alloc);
            for r in 0..t.nrows() {
                eval.charge(1, span)?;
                out.push(Value::Record(t.row_record(r).unwrap()));
            }
            Ok(out)
        }
        Value::Range(r) => r.collect_bounded(65_536, alloc).map_err(Control::Error),
        other => {
            let mut msg = AString::from_str_in("expected a list, table, or range, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_zip<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((other_val, other_span)) = args.pos.first() else {
        return Err(arity_err("zip", "expected a list, table, or range to zip with", span, alloc));
    };
    let left = iter_to_pvec(eval, input, span)?;
    let right = iter_to_pvec(eval, other_val.share(), *other_span)?;
    let n = left.len().min(right.len());
    let mut out = PVec::new(alloc);
    for i in 0..n {
        eval.charge(1, span)?;
        let mut pair = PVec::new(alloc);
        pair.push(left.get(i).unwrap().share());
        pair.push(right.get(i).unwrap().share());
        out.push(Value::List(pair));
    }
    Ok(Value::List(out))
}

fn builtin_each<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((cval, cspan)) = args.pos.first() else {
        return Err(arity_err("each", "expected a closure", span, alloc));
    };
    let closure = require_closure(cval, "each", *cspan, alloc).map_err(Control::Error)?;
    let items = iter_to_pvec(eval, input, span)?;
    let mut out = PVec::new(alloc);
    for i in 0..items.len() {
        eval.charge(1, span)?;
        let item = items.get(i).unwrap().share();
        let res = eval.invoke_closure(&closure, core::slice::from_ref(&item), item.share(), span)?;
        out.push(res);
    }
    if let Some(table) = Table::try_from_list(&out, alloc) {
        Ok(Value::Table(table))
    } else {
        Ok(Value::List(out))
    }
}

fn builtin_filter<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((cval, cspan)) = args.pos.first() else {
        return Err(arity_err("filter", "expected a closure", span, alloc));
    };
    let closure = require_closure(cval, "filter", *cspan, alloc).map_err(Control::Error)?;
    match input {
        Value::Table(t) => {
            let mut out = t.empty_like();
            for r in 0..t.nrows() {
                eval.charge(1, span)?;
                let rec = t.row_record(r).unwrap();
                let rv = Value::Record(rec.share());
                let res = eval.invoke_closure(&closure, core::slice::from_ref(&rv), rv.share(), span)?;
                if res.is_truthy() {
                    out.push_record(&rec);
                }
            }
            Ok(Value::Table(out))
        }
        other => {
            let items = iter_to_pvec(eval, other, span)?;
            let mut out = PVec::new(alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                let item = items.get(i).unwrap().share();
                let res =
                    eval.invoke_closure(&closure, core::slice::from_ref(&item), item.share(), span)?;
                if res.is_truthy() {
                    out.push(item);
                }
            }
            Ok(Value::List(out))
        }
    }
}

fn builtin_reduce<'a>(
    eval: &mut Evaluator<'a, '_>,
    mut args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let fold_init = args.take_flag_value("fold", "f");
    let Some((cval, cspan)) = args.pos.first() else {
        return Err(arity_err("reduce", "expected a closure", span, alloc));
    };
    let closure = require_closure(cval, "reduce", *cspan, alloc).map_err(Control::Error)?;
    let items = iter_to_pvec(eval, input, span)?;
    let (mut acc, start_idx) = if let Some(init) = fold_init {
        (init, 0usize)
    } else {
        if items.is_empty() {
            return Err(Control::Error(ShellError::new(
                ErrorKind::IndexOutOfBounds,
                span,
                "cannot `reduce` empty collection without `--fold` initial value",
                alloc,
            )));
        }
        (items.get(0).unwrap().share(), 1usize)
    };
    for i in start_idx..items.len() {
        eval.charge(1, span)?;
        let item = items.get(i).unwrap().share();
        let call_args = [item, acc.share()];
        acc = eval.invoke_closure(&closure, &call_args, acc, span)?;
    }
    Ok(acc)
}

fn builtin_group_by<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::Table(t) => {
            let Some((arg, aspan)) = args.pos.first() else {
                return Err(arity_err("group-by", "expected column name or closure", span, alloc));
            };
            let mut order: AVec<'a, AString<'a>> = AVec::new_in(alloc);
            let mut groups: AVec<'a, Table<'a>> = AVec::new_in(alloc);
            let col_idx = if matches!(arg, Value::Closure(_)) {
                None
            } else {
                let cname = value_as_col_name(arg, *aspan, alloc).map_err(Control::Error)?;
                let Some(ci) = t.col_index(cname.as_str()) else {
                    let mut msg = AString::from_str_in("table has no column `", alloc);
                    msg.push_str(cname.as_str());
                    msg.push('`');
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::MissingColumn,
                        *aspan,
                        msg.as_str(),
                        alloc,
                    )));
                };
                Some(ci)
            };
            for r in 0..t.nrows() {
                eval.charge(1, span)?;
                let rec = t.row_record(r).unwrap();
                let key_val = if let Some(ci) = col_idx {
                    t.cell(r, ci).unwrap().share()
                } else if let Value::Closure(c) = arg {
                    let rv = Value::Record(rec.share());
                    eval.invoke_closure(c, core::slice::from_ref(&rv), rv.share(), span)?
                } else {
                    Value::Nothing
                };
                let key_str = key_val.to_display_string(alloc);
                if let Some(pos) = order.iter().position(|k| k.as_str() == key_str.as_str()) {
                    groups[pos].push_record(&rec);
                } else {
                    let mut gt = Table::new(t.columns.share(), alloc);
                    gt.push_record(&rec);
                    order.push(key_str);
                    groups.push(gt);
                }
            }
            let mut out = Record::new(alloc);
            for (k, g) in order.iter().zip(groups.into_iter()) {
                out.insert_str(k.as_str(), Value::Table(g));
            }
            Ok(Value::Record(out))
        }
        Value::List(items) => {
            let mut order: AVec<'a, AString<'a>> = AVec::new_in(alloc);
            let mut groups: AVec<'a, PVec<'a, Value<'a>>> = AVec::new_in(alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                let item = items.get(i).unwrap().share();
                let key_val = match args.pos.first() {
                    None => item.share(),
                    Some((Value::Closure(c), _)) => {
                        eval.invoke_closure(c, core::slice::from_ref(&item), item.share(), span)?
                    }
                    Some((v, s)) => {
                        let cname = value_as_col_name(v, *s, alloc).map_err(Control::Error)?;
                        let member = PathMember::Field {
                            name: Rc::from_str_in(cname.as_str(), alloc),
                            optional: false,
                        };
                        item.follow_member(&member, *s, alloc).map_err(Control::Error)?
                    }
                };
                let key_str = key_val.to_display_string(alloc);
                if let Some(pos) = order.iter().position(|k| k.as_str() == key_str.as_str()) {
                    groups[pos].push(item);
                } else {
                    let mut gv = PVec::new(alloc);
                    gv.push(item);
                    order.push(key_str);
                    groups.push(gv);
                }
            }
            let mut out = Record::new(alloc);
            for (k, g) in order.iter().zip(groups.into_iter()) {
                out.insert_str(k.as_str(), Value::List(g));
            }
            Ok(Value::Record(out))
        }
        other => {
            let mut msg = AString::from_str_in("`group-by` expects a table or list, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpsertMode {
    Insert,
    Update,
    Upsert,
}

fn builtin_insert_update_upsert<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
    mode: UpsertMode,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let cmd = match mode {
        UpsertMode::Insert => "insert",
        UpsertMode::Update => "update",
        UpsertMode::Upsert => "upsert",
    };
    if args.pos.len() < 2 {
        return Err(arity_err(cmd, "expected <col> <value|closure>", span, alloc));
    }
    let cname = value_as_col_name(&args.pos[0].0, args.pos[0].1, alloc).map_err(Control::Error)?;
    let rhs = &args.pos[1].0;

    match input {
        Value::Record(mut rec) => {
            if mode == UpsertMode::Update && !rec.contains_key(cname.as_str()) {
                let mut msg = AString::from_str_in("record has no field `", alloc);
                msg.push_str(cname.as_str());
                msg.push('`');
                return Err(Control::Error(ShellError::new(
                    ErrorKind::MissingColumn,
                    args.pos[0].1,
                    msg.as_str(),
                    alloc,
                )));
            }
            let new_val = if let Value::Closure(c) = rhs {
                let arg = Value::Record(rec.share());
                let prev = rec.get(cname.as_str()).map(Share::share).unwrap_or(Value::Nothing);
                eval.invoke_closure(c, core::slice::from_ref(&arg), prev, span)?
            } else {
                rhs.share()
            };
            rec.insert_str(cname.as_str(), new_val);
            Ok(Value::Record(rec))
        }
        Value::Table(mut t) => {
            let existing = t.col_index(cname.as_str());
            if mode == UpsertMode::Update && existing.is_none() {
                let mut msg = AString::from_str_in("table has no column `", alloc);
                msg.push_str(cname.as_str());
                msg.push('`');
                return Err(Control::Error(ShellError::new(
                    ErrorKind::MissingColumn,
                    args.pos[0].1,
                    msg.as_str(),
                    alloc,
                )));
            }
            let mut new_col = PVec::new(alloc);
            for r in 0..t.nrows() {
                eval.charge(1, span)?;
                let val = if let Value::Closure(c) = rhs {
                    let rec = Value::Record(t.row_record(r).unwrap());
                    let prev = existing
                        .and_then(|ci| t.cell(r, ci))
                        .map(Share::share)
                        .unwrap_or(Value::Nothing);
                    eval.invoke_closure(c, core::slice::from_ref(&rec), prev, span)?
                } else {
                    rhs.share()
                };
                new_col.push(val);
            }
            if let Some(ci) = existing {
                t.cols.set(ci, new_col);
            } else {
                t.columns.push(Rc::from_str_in(cname.as_str(), alloc));
                t.cols.push(new_col);
            }
            Ok(Value::Table(t))
        }
        other => {
            let mut msg = AString::from_str_in("`", alloc);
            msg.push_str(cmd);
            msg.push_str("` expects a record or table, got ");
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_rename<'a>(
    eval: &Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.len() < 2 {
        return Err(arity_err("rename", "expected <old> <new>", span, alloc));
    }
    let old_name = value_as_col_name(&args.pos[0].0, args.pos[0].1, alloc).map_err(Control::Error)?;
    let new_name = value_as_col_name(&args.pos[1].0, args.pos[1].1, alloc).map_err(Control::Error)?;

    match input {
        Value::Table(mut t) => {
            let Some(ci) = t.col_index(old_name.as_str()) else {
                let mut msg = AString::from_str_in("table has no column `", alloc);
                msg.push_str(old_name.as_str());
                msg.push('`');
                return Err(Control::Error(ShellError::new(
                    ErrorKind::MissingColumn,
                    args.pos[0].1,
                    msg.as_str(),
                    alloc,
                )));
            };
            t.columns.set(ci, Rc::from_str_in(new_name.as_str(), alloc));
            Ok(Value::Table(t))
        }
        Value::Record(rec) => {
            if !rec.contains_key(old_name.as_str()) {
                let mut msg = AString::from_str_in("record has no field `", alloc);
                msg.push_str(old_name.as_str());
                msg.push('`');
                return Err(Control::Error(ShellError::new(
                    ErrorKind::MissingColumn,
                    args.pos[0].1,
                    msg.as_str(),
                    alloc,
                )));
            }
            let mut out = Record::new(alloc);
            for (k, v) in rec.iter() {
                if k == old_name.as_str() {
                    out.insert_str(new_name.as_str(), v.share());
                } else {
                    out.insert_str(k, v.share());
                }
            }
            Ok(Value::Record(out))
        }
        other => {
            let mut msg = AString::from_str_in("`rename` expects a table or record, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

// =============================================================================
// 3. Math Builtins
// =============================================================================

fn math_unary_scalar<'a>(op: &str, v: &Value<'a>, span: Span, alloc: Alloc<'a>) -> Result<Value<'a>, Control<'a>> {
    match (op, v) {
        ("abs", Value::Int(n)) => n.checked_abs().map(Value::Int).ok_or_else(|| {
            Control::Error(ShellError::new(ErrorKind::Overflow, span, "integer overflow in `math abs`", alloc))
        }),
        ("abs", Value::Float(f)) => Ok(Value::Float(f64_abs(*f))),
        ("abs", Value::Size(Bytes(n))) => n.checked_abs().map(|x| Value::Size(Bytes(x))).ok_or_else(|| {
            Control::Error(ShellError::new(ErrorKind::Overflow, span, "size overflow in `math abs`", alloc))
        }),
        ("abs", Value::Duration(Nanos(n))) => n.checked_abs().map(|x| Value::Duration(Nanos(x))).ok_or_else(|| {
            Control::Error(ShellError::new(ErrorKind::Overflow, span, "duration overflow in `math abs`", alloc))
        }),
        ("round", Value::Int(n)) => Ok(Value::Int(*n)),
        ("round", Value::Float(f)) => Ok(Value::Int(f64_round(*f) as i64)),
        ("floor", Value::Int(n)) => Ok(Value::Int(*n)),
        ("floor", Value::Float(f)) => Ok(Value::Int(f64_floor(*f) as i64)),
        ("ceil", Value::Int(n)) => Ok(Value::Int(*n)),
        ("ceil", Value::Float(f)) => Ok(Value::Int(f64_ceil(*f) as i64)),
        (_, other) => {
            let mut msg = AString::from_str_in("`math ", alloc);
            msg.push_str(op);
            msg.push_str("` does not support ");
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_math<'a>(
    eval: &mut Evaluator<'a, '_>,
    sub: &str,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if matches!(sub, "abs" | "round" | "floor" | "ceil") {
        return match input {
            Value::List(items) => {
                let mut out = PVec::new(alloc);
                for i in 0..items.len() {
                    eval.charge(1, span)?;
                    out.push(math_unary_scalar(sub, items.get(i).unwrap(), span, alloc)?);
                }
                Ok(Value::List(out))
            }
            scalar => math_unary_scalar(sub, &scalar, span, alloc),
        };
    }

    let items = match input {
        Value::List(l) => l,
        Value::Range(r) => r.collect_bounded(65_536, alloc).map_err(Control::Error)?,
        other => {
            let mut msg = AString::from_str_in("`math ", alloc);
            msg.push_str(sub);
            msg.push_str("` expects a list or range, got ");
            msg.push_str(other.type_name());
            return Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)));
        }
    };

    match sub {
        "sum" => {
            if items.is_empty() {
                return Ok(Value::Int(0));
            }
            match items.get(0).unwrap() {
                Value::Size(_) => {
                    let mut total = 0i64;
                    for i in 0..items.len() {
                        eval.charge(1, span)?;
                        let Value::Size(Bytes(n)) = items.get(i).unwrap() else {
                            return Err(type_err("mixed types in `math sum`", span, alloc));
                        };
                        total = total.checked_add(*n).ok_or_else(|| {
                            Control::Error(ShellError::new(ErrorKind::Overflow, span, "overflow in `math sum`", alloc))
                        })?;
                    }
                    Ok(Value::Size(Bytes(total)))
                }
                Value::Duration(_) => {
                    let mut total = 0i64;
                    for i in 0..items.len() {
                        eval.charge(1, span)?;
                        let Value::Duration(Nanos(n)) = items.get(i).unwrap() else {
                            return Err(type_err("mixed types in `math sum`", span, alloc));
                        };
                        total = total.checked_add(*n).ok_or_else(|| {
                            Control::Error(ShellError::new(ErrorKind::Overflow, span, "overflow in `math sum`", alloc))
                        })?;
                    }
                    Ok(Value::Duration(Nanos(total)))
                }
                Value::Int(_) | Value::Float(_) => {
                    let mut int_sum = 0i64;
                    let mut float_sum = 0.0f64;
                    let mut is_float = false;
                    for i in 0..items.len() {
                        eval.charge(1, span)?;
                        match items.get(i).unwrap() {
                            Value::Int(n) => {
                                if is_float {
                                    float_sum += *n as f64;
                                } else if let Some(next) = int_sum.checked_add(*n) {
                                    int_sum = next;
                                } else {
                                    is_float = true;
                                    float_sum = (int_sum as f64) + (*n as f64);
                                }
                            }
                            Value::Float(f) => {
                                if !is_float {
                                    is_float = true;
                                    float_sum = int_sum as f64;
                                }
                                float_sum += *f;
                            }
                            _ => return Err(type_err("non-numeric element in `math sum`", span, alloc)),
                        }
                    }
                    Ok(if is_float { Value::Float(float_sum) } else { Value::Int(int_sum) })
                }
                _ => Err(type_err("`math sum` expects numbers, sizes, or durations", span, alloc)),
            }
        }
        "avg" => {
            if items.is_empty() {
                return Err(Control::Error(ShellError::new(
                    ErrorKind::IndexOutOfBounds,
                    span,
                    "cannot take `math avg` of empty list",
                    alloc,
                )));
            }
            let n = items.len() as i64;
            match items.get(0).unwrap() {
                Value::Size(_) => {
                    let mut total = 0i128;
                    for i in 0..items.len() {
                        eval.charge(1, span)?;
                        let Value::Size(Bytes(x)) = items.get(i).unwrap() else {
                            return Err(type_err("mixed types in `math avg`", span, alloc));
                        };
                        total += i128::from(*x);
                    }
                    Ok(Value::Size(Bytes((total / i128::from(n)) as i64)))
                }
                Value::Duration(_) => {
                    let mut total = 0i128;
                    for i in 0..items.len() {
                        eval.charge(1, span)?;
                        let Value::Duration(Nanos(x)) = items.get(i).unwrap() else {
                            return Err(type_err("mixed types in `math avg`", span, alloc));
                        };
                        total += i128::from(*x);
                    }
                    Ok(Value::Duration(Nanos((total / i128::from(n)) as i64)))
                }
                _ => {
                    let mut sum = 0.0f64;
                    for i in 0..items.len() {
                        eval.charge(1, span)?;
                        let Some(f) = items.get(i).unwrap().as_float() else {
                            return Err(type_err("non-numeric element in `math avg`", span, alloc));
                        };
                        sum += f;
                    }
                    Ok(Value::Float(sum / (items.len() as f64)))
                }
            }
        }
        "min" | "max" => {
            if items.is_empty() {
                return Err(Control::Error(ShellError::new(
                    ErrorKind::IndexOutOfBounds,
                    span,
                    "cannot compute min/max of empty list",
                    alloc,
                )));
            }
            let want_max = sub == "max";
            let mut best = items.get(0).unwrap().share();
            for i in 1..items.len() {
                eval.charge(1, span)?;
                let cur = items.get(i).unwrap();
                let Some(ord) = cur.compare(&best) else {
                    return Err(type_err("incomparable elements in `math min/max`", span, alloc));
                };
                if (want_max && ord == Ordering::Greater) || (!want_max && ord == Ordering::Less) {
                    best = cur.share();
                }
            }
            Ok(best)
        }
        "median" => {
            if items.is_empty() {
                return Err(Control::Error(ShellError::new(
                    ErrorKind::IndexOutOfBounds,
                    span,
                    "cannot compute `math median` of empty list",
                    alloc,
                )));
            }
            let mut sorted = items.to_avec();
            sorted.sort_unstable_by(|a, b| compare_values_for_sort(a, b, false, alloc));
            let mid = sorted.len() / 2;
            if sorted.len() % 2 == 1 {
                Ok(sorted[mid].share())
            } else {
                match (&sorted[mid - 1], &sorted[mid]) {
                    (Value::Size(Bytes(a)), Value::Size(Bytes(b))) => {
                        Ok(Value::Size(Bytes(((i128::from(*a) + i128::from(*b)) / 2) as i64)))
                    }
                    (Value::Duration(Nanos(a)), Value::Duration(Nanos(b))) => {
                        Ok(Value::Duration(Nanos(((i128::from(*a) + i128::from(*b)) / 2) as i64)))
                    }
                    (a, b) => match (a.as_float(), b.as_float()) {
                        (Some(fa), Some(fb)) => Ok(Value::Float((fa + fb) * 0.5)),
                        _ => Err(type_err("non-numeric element in `math median`", span, alloc)),
                    },
                }
            }
        }
        _ => {
            let mut msg = AString::from_str_in("unknown `math` subcommand `", alloc);
            msg.push_str(sub);
            msg.push('`');
            Err(Control::Error(ShellError::new(ErrorKind::Undefined, span, msg.as_str(), alloc)))
        }
    }
}

// =============================================================================
// 4. String & Parsing Builtins
// =============================================================================

fn map_str_or_list<'a>(
    eval: &mut Evaluator<'a, '_>,
    input: Value<'a>,
    span: Span,
    mut f: impl FnMut(&str, Alloc<'a>) -> Result<Value<'a>, Control<'a>>,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match input {
        Value::String(s) => f(s.as_ref(), alloc),
        Value::List(items) => {
            let mut out = PVec::new(alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                let Some(s) = items.get(i).unwrap().as_str() else {
                    return Err(type_err("expected string elements in list", span, alloc));
                };
                out.push(f(s, alloc)?);
            }
            Ok(Value::List(out))
        }
        other => {
            let mut msg = AString::from_str_in("expected string or list of strings, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_str<'a>(
    eval: &mut Evaluator<'a, '_>,
    sub: &str,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match sub {
        "trim" => {
            let left = args.has_flag("left", "l");
            let right = args.has_flag("right", "r");
            let trim_char = match args.flag_value("char", "c") {
                Some(v) => {
                    let s = require_string(v, "--char", span, alloc).map_err(Control::Error)?;
                    s.as_str().chars().next()
                }
                None => None,
            };
            map_str_or_list(eval, input, span, |s, a| {
                let res = match (left, right, trim_char) {
                    (true, false, Some(ch)) => s.trim_start_matches(ch),
                    (false, true, Some(ch)) => s.trim_end_matches(ch),
                    (_, _, Some(ch)) => s.trim_matches(ch),
                    (true, false, None) => s.trim_start(),
                    (false, true, None) => s.trim_end(),
                    (_, _, None) => s.trim(),
                };
                Ok(Value::string(res, a))
            })
        }
        "replace" => {
            if args.pos.len() < 2 {
                return Err(arity_err("str replace", "expected <find> <replace>", span, alloc));
            }
            let all = args.has_flag("all", "a");
            let find = require_string(&args.pos[0].0, "find", args.pos[0].1, alloc).map_err(Control::Error)?;
            let repl = require_string(&args.pos[1].0, "replace", args.pos[1].1, alloc).map_err(Control::Error)?;
            map_str_or_list(eval, input, span, |s, a| {
                let r = str_replace_in(s, find.as_str(), repl.as_str(), all, a);
                Ok(Value::string(r.as_str(), a))
            })
        }
        "split" => {
            let Some((v, s)) = args.pos.first() else {
                return Err(arity_err("str split", "expected <separator>", span, alloc));
            };
            let sep = require_string(v, "separator", *s, alloc).map_err(Control::Error)?;
            let Some(text) = input.as_str() else {
                return Err(type_err("`str split` expects a string", span, alloc));
            };
            let mut out = PVec::new(alloc);
            for part in text.split(sep.as_str()) {
                eval.charge(1, span)?;
                out.push(Value::string(part, alloc));
            }
            Ok(Value::List(out))
        }
        "join" => {
            let sep = match args.pos.first() {
                Some((v, s)) => require_string(v, "separator", *s, alloc).map_err(Control::Error)?,
                None => AString::new_in(alloc),
            };
            let items = iter_to_pvec(eval, input, span)?;
            let mut out = AString::new_in(alloc);
            for i in 0..items.len() {
                eval.charge(1, span)?;
                if i > 0 {
                    out.push_str(sep.as_str());
                }
                let _ = items.get(i).unwrap().format_cell(&mut out);
            }
            Ok(Value::string(out.as_str(), alloc))
        }
        "upcase" => map_str_or_list(eval, input, span, |s, a| {
            let up = str_to_uppercase(s, a);
            Ok(Value::string(up.as_str(), a))
        }),
        "downcase" => map_str_or_list(eval, input, span, |s, a| {
            let dn = str_to_lowercase(s, a);
            Ok(Value::string(dn.as_str(), a))
        }),
        "contains" => {
            let Some((v, s)) = args.pos.first() else {
                return Err(arity_err("str contains", "expected <substring>", span, alloc));
            };
            let ignore_case = args.has_flag("ignore-case", "i");
            let needle = require_string(v, "substring", *s, alloc).map_err(Control::Error)?;
            let needle_lc = if ignore_case { Some(str_to_lowercase(needle.as_str(), alloc)) } else { None };
            map_str_or_list(eval, input, span, |s, a| {
                let matched = if let Some(nlc) = &needle_lc {
                    str_to_lowercase(s, a).as_str().contains(nlc.as_str())
                } else {
                    s.contains(needle.as_str())
                };
                Ok(Value::Bool(matched))
            })
        }
        "starts-with" => {
            let Some((v, s)) = args.pos.first() else {
                return Err(arity_err("str starts-with", "expected <prefix>", span, alloc));
            };
            let prefix = require_string(v, "prefix", *s, alloc).map_err(Control::Error)?;
            map_str_or_list(eval, input, span, |s, _| Ok(Value::Bool(s.starts_with(prefix.as_str()))))
        }
        "ends-with" => {
            let Some((v, s)) = args.pos.first() else {
                return Err(arity_err("str ends-with", "expected <suffix>", span, alloc));
            };
            let suffix = require_string(v, "suffix", *s, alloc).map_err(Control::Error)?;
            map_str_or_list(eval, input, span, |s, _| Ok(Value::Bool(s.ends_with(suffix.as_str()))))
        }
        "length" => map_str_or_list(eval, input, span, |s, _| Ok(Value::Int(s.chars().count() as i64))),
        "substring" => {
            if args.pos.is_empty() {
                return Err(arity_err("str substring", "expected range or <start> <end>", span, alloc));
            }
            let (start, end, inclusive) = match &args.pos[0].0 {
                Value::Range(r) => (r.start, r.end, r.inclusive),
                Value::Int(st) => {
                    let en = match args.pos.get(1) {
                        Some((v, s)) => Some(require_int(v, "end", *s, alloc).map_err(Control::Error)?),
                        None => None,
                    };
                    (*st, en, false)
                }
                other => {
                    let mut msg = AString::from_str_in("`str substring` expects a range or ints, got ", alloc);
                    msg.push_str(other.type_name());
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::Type,
                        args.pos[0].1,
                        msg.as_str(),
                        alloc,
                    )));
                }
            };
            map_str_or_list(eval, input, span, |s, a| {
                let total = s.chars().count() as i64;
                let s_idx = if start < 0 { (total + start).max(0) } else { start.min(total) } as usize;
                let e_idx = match end {
                    None => total as usize,
                    Some(e) => {
                        let norm = if e < 0 { (total + e).max(0) } else { e };
                        let excl = if inclusive { norm + 1 } else { norm };
                        excl.clamp(0, total) as usize
                    }
                };
                let mut out = AString::new_in(a);
                if s_idx < e_idx {
                    for ch in s.chars().skip(s_idx).take(e_idx - s_idx) {
                        out.push(ch);
                    }
                }
                Ok(Value::string(out.as_str(), a))
            })
        }
        "index-of" => {
            let Some((v, s)) = args.pos.first() else {
                return Err(arity_err("str index-of", "expected <substring>", span, alloc));
            };
            let needle = require_string(v, "substring", *s, alloc).map_err(Control::Error)?;
            map_str_or_list(eval, input, span, |s, _| {
                let idx = match s.find(needle.as_str()) {
                    Some(byte_pos) => s[..byte_pos].chars().count() as i64,
                    None => -1,
                };
                Ok(Value::Int(idx))
            })
        }
        _ => {
            let mut msg = AString::from_str_in("unknown `str` subcommand `", alloc);
            msg.push_str(sub);
            msg.push('`');
            Err(Control::Error(ShellError::new(ErrorKind::Undefined, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_lines<'a>(
    eval: &mut Evaluator<'a, '_>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some(text) = input.as_str() else {
        return Err(type_err("`lines` expects a string", span, alloc));
    };
    let mut out = PVec::new(alloc);
    for line in text.lines() {
        eval.charge(1, span)?;
        out.push(Value::string(line, alloc));
    }
    Ok(Value::List(out))
}

fn builtin_split_row<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((v, s)) = args.pos.first() else {
        return Err(arity_err("split row", "expected <separator>", span, alloc));
    };
    let sep = require_string(v, "separator", *s, alloc).map_err(Control::Error)?;
    let Some(text) = input.as_str() else {
        return Err(type_err("`split row` expects a string", span, alloc));
    };
    let mut out = PVec::new(alloc);
    for part in text.split(sep.as_str()) {
        eval.charge(1, span)?;
        out.push(Value::string(part, alloc));
    }
    Ok(Value::List(out))
}

fn builtin_split_column<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((v, s)) = args.pos.first() else {
        return Err(arity_err("split column", "expected <separator> [col_names...]", span, alloc));
    };
    let sep = require_string(v, "separator", *s, alloc).map_err(Control::Error)?;
    let mut custom_cols = AVec::new_in(alloc);
    for (cv, cs) in args.pos.iter().skip(1) {
        custom_cols.push(value_as_col_name(cv, *cs, alloc).map_err(Control::Error)?);
    }

    let mut lines: AVec<'a, AString<'a>> = AVec::new_in(alloc);
    match &input {
        Value::String(text) => {
            for line in text.as_ref().lines() {
                eval.charge(1, span)?;
                lines.push(AString::from_str_in(line, alloc));
            }
        }
        Value::List(items) => {
            for i in 0..items.len() {
                eval.charge(1, span)?;
                lines.push(items.get(i).unwrap().to_display_string(alloc));
            }
        }
        other => {
            let mut msg = AString::from_str_in("`split column` expects a string or list, got ", alloc);
            msg.push_str(other.type_name());
            return Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)));
        }
    }

    let mut split_rows: AVec<'a, AVec<'a, Value<'a>>> = AVec::with_capacity_in(lines.len(), alloc);
    let mut max_cols = custom_cols.len();
    for line in lines.iter() {
        eval.charge(1, span)?;
        let mut row = AVec::new_in(alloc);
        for part in line.as_str().split(sep.as_str()) {
            row.push(Value::string(part, alloc));
        }
        if row.len() > max_cols {
            max_cols = row.len();
        }
        split_rows.push(row);
    }

    let mut headers = PVec::new(alloc);
    for i in 0..max_cols {
        if let Some(c) = custom_cols.get(i) {
            headers.push(Rc::from_str_in(c.as_str(), alloc));
        } else {
            let mut cname = AString::new_in(alloc);
            let _ = write!(cname, "column{i}");
            headers.push(Rc::from_str_in(cname.as_str(), alloc));
        }
    }
    let mut table = Table::new(headers, alloc);
    for row in split_rows.iter() {
        table.push_row_values(row.as_slice());
    }
    Ok(Value::Table(table))
}

enum ParseToken<'a> {
    Literal(AString<'a>),
    Capture,
}

fn compile_parse_template<'a>(
    template: &str,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<(AVec<'a, ParseToken<'a>>, PVec<'a, ARc<'a, str>>), ShellError<'a>> {
    let mut tokens = AVec::new_in(alloc);
    let mut cols = PVec::new(alloc);
    let mut lit = AString::new_in(alloc);
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            if chars.peek() == Some(&'{') {
                chars.next();
                lit.push('{');
                continue;
            }
            if !lit.is_empty() {
                tokens.push(ParseToken::Literal(lit));
                lit = AString::new_in(alloc);
            }
            let mut name = AString::new_in(alloc);
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '}' {
                    closed = true;
                    break;
                }
                name.push(c);
            }
            if !closed || name.is_empty() {
                return Err(ShellError::new(
                    ErrorKind::Format,
                    span,
                    "invalid `{field}` capture in `parse` template",
                    alloc,
                ));
            }
            cols.push(Rc::from_str_in(name.as_str(), alloc));
            tokens.push(ParseToken::Capture);
        } else if ch == '}' && chars.peek() == Some(&'}') {
            chars.next();
            lit.push('}');
        } else {
            lit.push(ch);
        }
    }
    if !lit.is_empty() {
        tokens.push(ParseToken::Literal(lit));
    }
    Ok((tokens, cols))
}

fn match_parse_line<'a>(
    line: &str,
    tokens: &[ParseToken<'a>],
    alloc: Alloc<'a>,
) -> Option<AVec<'a, Value<'a>>> {
    let mut captures = AVec::new_in(alloc);
    let mut pos = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        match &tokens[i] {
            ParseToken::Literal(lit) => {
                if !line[pos..].starts_with(lit.as_str()) {
                    return None;
                }
                pos += lit.len();
                i += 1;
            }
            ParseToken::Capture => {
                // Find the next literal delimiter (if any).
                if let Some(ParseToken::Literal(next_lit)) = tokens.get(i + 1) {
                    let rel = line[pos..].find(next_lit.as_str())?;
                    captures.push(Value::string(&line[pos..pos + rel], alloc));
                    pos += rel;
                } else {
                    // Last token in template captures the remainder of the line.
                    captures.push(Value::string(&line[pos..], alloc));
                    pos = line.len();
                }
                i += 1;
            }
        }
    }
    if pos == line.len() {
        Some(captures)
    } else {
        None
    }
}

fn builtin_parse<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((v, s)) = args.pos.first() else {
        return Err(arity_err("parse", "expected template pattern like \"{a} {b}\"", span, alloc));
    };
    let pattern = require_string(v, "template", *s, alloc).map_err(Control::Error)?;
    let (tokens, cols) = compile_parse_template(pattern.as_str(), *s, alloc).map_err(Control::Error)?;
    let mut table = Table::new(cols, alloc);

    match input {
        Value::String(text) => {
            for line in text.as_ref().lines() {
                eval.charge(1, span)?;
                if let Some(row) = match_parse_line(line, tokens.as_slice(), alloc) {
                    table.push_row_values(row.as_slice());
                }
            }
            Ok(Value::Table(table))
        }
        Value::List(items) => {
            for i in 0..items.len() {
                eval.charge(1, span)?;
                let line = items.get(i).unwrap().to_display_string(alloc);
                if let Some(row) = match_parse_line(line.as_str(), tokens.as_slice(), alloc) {
                    table.push_row_values(row.as_slice());
                }
            }
            Ok(Value::Table(table))
        }
        other => {
            let mut msg = AString::from_str_in("`parse` expects a string or list of strings, got ", alloc);
            msg.push_str(other.type_name());
            Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
        }
    }
}

// =============================================================================
// 5. Conversions & Formats
// =============================================================================

fn parse_int_str(s: &str) -> Option<i64> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        i64::from_str_radix(bin, 2).ok()
    } else if let Some(oct) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        i64::from_str_radix(oct, 8).ok()
    } else {
        t.parse::<i64>().ok()
    }
}

fn parse_size_str<'a>(s: &str, alloc: Alloc<'a>) -> Option<Bytes> {
    let t = s.trim();
    let split_at = t
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
        .unwrap_or(t.len());
    let num_str = t[..split_at].trim();
    let unit = str_to_lowercase(t[split_at..].trim(), alloc);
    let mult: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "kb" => 1_000.0,
        "mb" => 1_000_000.0,
        "gb" => 1_000_000_000.0,
        "tb" => 1_000_000_000_000.0,
        "kib" => 1_024.0,
        "mib" => 1_048_576.0,
        "gib" => 1_073_741_824.0,
        "tib" => 1_099_511_627_776.0,
        _ => return None,
    };
    if let Ok(i) = num_str.parse::<i64>() {
        Some(Bytes((i as i128).saturating_mul(mult as i128) as i64))
    } else if let Ok(f) = num_str.parse::<f64>() {
        Some(Bytes(f64_round(f * mult) as i64))
    } else {
        None
    }
}

fn parse_duration_str<'a>(s: &str, alloc: Alloc<'a>) -> Option<Nanos> {
    let t = s.trim();
    let split_at = t
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
        .unwrap_or(t.len());
    let num_str = t[..split_at].trim();
    let unit = str_to_lowercase(t[split_at..].trim(), alloc);
    let mult: f64 = match unit.as_str() {
        "ns" => 1.0,
        "us" | "µs" => 1_000.0,
        "ms" => 1_000_000.0,
        "s" | "sec" => 1_000_000_000.0,
        "min" => 60_000_000_000.0,
        "h" | "hr" => 3_600_000_000_000.0,
        "d" | "day" => 86_400_000_000_000.0,
        "wk" => 604_800_000_000_000.0,
        _ => return None,
    };
    if let Ok(i) = num_str.parse::<i64>() {
        Some(Nanos((i as i128).saturating_mul(mult as i128) as i64))
    } else if let Ok(f) = num_str.parse::<f64>() {
        Some(Nanos(f64_round(f * mult) as i64))
    } else {
        None
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = month as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + (day as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

fn parse_iso_datetime(s: &str) -> Option<DateTime> {
    let t = s.trim();
    if t.len() < 10 {
        return None;
    }
    let b = t.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = t[0..4].parse::<i64>().ok()?;
    let month = t[5..7].parse::<u32>().ok()?;
    let day = t[8..10].parse::<u32>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut hour = 0i64;
    let mut minute = 0i64;
    let mut second = 0i64;
    let mut offset_secs = 0i32;

    if t.len() >= 19 && (b[10] == b'T' || b[10] == b' ' || b[10] == b't') {
        if b[13] != b':' || b[16] != b':' {
            return None;
        }
        hour = t[11..13].parse::<i64>().ok()?;
        minute = t[14..16].parse::<i64>().ok()?;
        second = t[17..19].parse::<i64>().ok()?;
        let rest = &t[19..];
        if let Some(tz) = rest.strip_prefix('Z').or_else(|| rest.strip_prefix('z')) {
            let _ = tz;
        } else if rest.len() >= 6 && (rest.starts_with('+') || rest.starts_with('-')) {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            let oh = rest[1..3].parse::<i32>().ok()?;
            let om = rest[4..6].parse::<i32>().ok()?;
            offset_secs = sign * (oh * 3600 + om * 60);
        }
    }
    let days = days_from_civil(year, month, day);
    let local_secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    let utc_secs = local_secs - i64::from(offset_secs);
    Some(DateTime { utc_ns: utc_secs.saturating_mul(1_000_000_000), offset_secs })
}

fn convert_into_scalar<'a>(
    sub: &str,
    v: &Value<'a>,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<Value<'a>, Control<'a>> {
    match sub {
        "int" => match v {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Float(f) => Ok(Value::Int(f64_trunc(*f) as i64)),
            Value::Bool(b) => Ok(Value::Int(i64::from(*b))),
            Value::Size(Bytes(n)) => Ok(Value::Int(*n)),
            Value::Duration(Nanos(n)) => Ok(Value::Int(*n)),
            Value::Date(dt) => Ok(Value::Int(dt.utc_ns)),
            Value::String(s) => parse_int_str(s.as_ref()).map(Value::Int).ok_or_else(|| {
                Control::Error(ShellError::new(
                    ErrorKind::Format,
                    span,
                    "cannot parse string as int",
                    alloc,
                ))
            }),
            other => {
                let mut msg = AString::from_str_in("cannot convert ", alloc);
                msg.push_str(other.type_name());
                msg.push_str(" into int");
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        },
        "float" => match v {
            Value::Float(f) => Ok(Value::Float(*f)),
            Value::Int(n) => Ok(Value::Float(*n as f64)),
            Value::String(s) => s.as_ref().trim().parse::<f64>().map(Value::Float).map_err(|_| {
                Control::Error(ShellError::new(
                    ErrorKind::Format,
                    span,
                    "cannot parse string as float",
                    alloc,
                ))
            }),
            other => {
                let mut msg = AString::from_str_in("cannot convert ", alloc);
                msg.push_str(other.type_name());
                msg.push_str(" into float");
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        },
        "string" => match v {
            Value::String(s) => Ok(Value::String(s.share())),
            Value::Binary(b) => core::str::from_utf8(b.as_ref())
                .map(|s| Value::string(s, alloc))
                .map_err(|_| {
                    Control::Error(ShellError::new(
                        ErrorKind::Format,
                        span,
                        "binary is not valid UTF-8",
                        alloc,
                    ))
                }),
            other => {
                let s = other.to_display_string(alloc);
                Ok(Value::string(s.as_str(), alloc))
            }
        },
        "size" | "filesize" => match v {
            Value::Size(b) => Ok(Value::Size(*b)),
            Value::Int(n) => Ok(Value::Size(Bytes(*n))),
            Value::Float(f) => Ok(Value::Size(Bytes(f64_round(*f) as i64))),
            Value::String(s) => parse_size_str(s.as_ref(), alloc).map(Value::Size).ok_or_else(|| {
                Control::Error(ShellError::new(
                    ErrorKind::Format,
                    span,
                    "cannot parse string as size",
                    alloc,
                ))
            }),
            other => {
                let mut msg = AString::from_str_in("cannot convert ", alloc);
                msg.push_str(other.type_name());
                msg.push_str(" into size");
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        },
        "duration" => match v {
            Value::Duration(d) => Ok(Value::Duration(*d)),
            Value::Int(n) => Ok(Value::Duration(Nanos(*n))),
            Value::String(s) => {
                parse_duration_str(s.as_ref(), alloc).map(Value::Duration).ok_or_else(|| {
                    Control::Error(ShellError::new(
                        ErrorKind::Format,
                        span,
                        "cannot parse string as duration",
                        alloc,
                    ))
                })
            }
            other => {
                let mut msg = AString::from_str_in("cannot convert ", alloc);
                msg.push_str(other.type_name());
                msg.push_str(" into duration");
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        },
        "bool" => match v {
            Value::Bool(b) => Ok(Value::Bool(*b)),
            Value::Int(n) => Ok(Value::Bool(*n != 0)),
            Value::Float(f) => Ok(Value::Bool(*f != 0.0 && !f.is_nan())),
            Value::String(s) => match str_to_lowercase(s.as_ref().trim(), alloc).as_str() {
                "true" | "1" | "yes" => Ok(Value::Bool(true)),
                "false" | "0" | "no" => Ok(Value::Bool(false)),
                _ => Err(Control::Error(ShellError::new(
                    ErrorKind::Format,
                    span,
                    "cannot parse string as bool",
                    alloc,
                ))),
            },
            other => {
                let mut msg = AString::from_str_in("cannot convert ", alloc);
                msg.push_str(other.type_name());
                msg.push_str(" into bool");
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        },
        "binary" => match v {
            Value::Binary(b) => Ok(Value::Binary(b.share())),
            Value::String(s) => Ok(Value::binary(s.as_bytes(), alloc)),
            Value::Int(n) => Ok(Value::binary(&n.to_le_bytes(), alloc)),
            other => {
                let mut msg = AString::from_str_in("cannot convert ", alloc);
                msg.push_str(other.type_name());
                msg.push_str(" into binary");
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        },
        "datetime" | "date" => match v {
            Value::Date(dt) => Ok(Value::Date(*dt)),
            Value::Int(n) => Ok(Value::Date(DateTime { utc_ns: *n, offset_secs: 0 })),
            Value::String(s) => parse_iso_datetime(s.as_ref()).map(Value::Date).ok_or_else(|| {
                Control::Error(ShellError::new(
                    ErrorKind::Format,
                    span,
                    "cannot parse string as ISO-8601 datetime",
                    alloc,
                ))
            }),
            other => {
                let mut msg = AString::from_str_in("cannot convert ", alloc);
                msg.push_str(other.type_name());
                msg.push_str(" into datetime");
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        },
        _ => {
            let mut msg = AString::from_str_in("unknown `into` target `", alloc);
            msg.push_str(sub);
            msg.push('`');
            Err(Control::Error(ShellError::new(ErrorKind::Undefined, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_into<'a>(
    eval: &mut Evaluator<'a, '_>,
    sub: &str,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if let Value::List(items) = input {
        let mut out = PVec::new(alloc);
        for i in 0..items.len() {
            eval.charge(1, span)?;
            out.push(convert_into_scalar(sub, items.get(i).unwrap(), span, alloc)?);
        }
        return Ok(Value::List(out));
    }
    convert_into_scalar(sub, &input, span, alloc)
}

fn builtin_to<'a>(
    eval: &Evaluator<'a, '_>,
    sub: &str,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match sub {
        "json" => {
            let raw = args.has_flag("raw", "r");
            let s = to_json_string(&input, raw, span, alloc).map_err(Control::Error)?;
            Ok(Value::string(s.as_str(), alloc))
        }
        "csv" => {
            let s = to_csv_string(&input, span, alloc).map_err(Control::Error)?;
            Ok(Value::string(s.as_str(), alloc))
        }
        "toml" => {
            let s = to_toml_string(&input, span, alloc).map_err(Control::Error)?;
            Ok(Value::string(s.as_str(), alloc))
        }
        "text" => {
            let s = to_text_string(&input, alloc);
            Ok(Value::string(s.as_str(), alloc))
        }
        _ => {
            let mut msg = AString::from_str_in("unknown `to` format `", alloc);
            msg.push_str(sub);
            msg.push('`');
            Err(Control::Error(ShellError::new(ErrorKind::Undefined, span, msg.as_str(), alloc)))
        }
    }
}

fn builtin_from<'a>(
    eval: &mut Evaluator<'a, '_>,
    sub: &str,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    match sub {
        "json" => match &input {
            Value::String(s) => from_json_bytes(s.as_bytes(), span, alloc).map_err(Control::Error),
            Value::Binary(b) => from_json_bytes(b.as_ref(), span, alloc).map_err(Control::Error),
            other => {
                let mut msg = AString::from_str_in("`from json` expects string or binary, got ", alloc);
                msg.push_str(other.type_name());
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
            }
        },
        "toml" => {
            let text = match &input {
                Value::String(s) => s.as_ref(),
                Value::Binary(b) => core::str::from_utf8(b.as_ref()).map_err(|_| {
                    Control::Error(ShellError::new(
                        ErrorKind::Format,
                        span,
                        "TOML input is not valid UTF-8",
                        alloc,
                    ))
                })?,
                other => {
                    let mut msg = AString::from_str_in("`from toml` expects string or binary, got ", alloc);
                    msg.push_str(other.type_name());
                    return Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)));
                }
            };
            from_toml_str(text, span, alloc).map_err(Control::Error)
        }
        "csv" => {
            let no_headers = args.has_flag("noheaders", "n");
            let sep = match args.flag_value("separator", "s") {
                Some(v) => {
                    let s = require_string(v, "--separator", span, alloc).map_err(Control::Error)?;
                    s.as_str().chars().next().unwrap_or(',')
                }
                None => ',',
            };
            let text = match &input {
                Value::String(s) => s.as_ref(),
                Value::Binary(b) => core::str::from_utf8(b.as_ref()).map_err(|_| {
                    Control::Error(ShellError::new(
                        ErrorKind::Format,
                        span,
                        "CSV input is not valid UTF-8",
                        alloc,
                    ))
                })?,
                other => {
                    let mut msg = AString::from_str_in("`from csv` expects string or binary, got ", alloc);
                    msg.push_str(other.type_name());
                    return Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)));
                }
            };
            Ok(Value::Table(parse_csv(text, no_headers, sep, alloc)))
        }
        "lines" => builtin_lines(eval, input, span),
        _ => {
            let mut msg = AString::from_str_in("unknown `from` format `", alloc);
            msg.push_str(sub);
            msg.push('`');
            Err(Control::Error(ShellError::new(ErrorKind::Undefined, span, msg.as_str(), alloc)))
        }
    }
}

// =============================================================================
// 6. Core / Meta / Session Builtins
// =============================================================================

fn builtin_describe<'a>(eval: &Evaluator<'a, '_>, input: Value<'a>) -> Value<'a> {
    let alloc = eval.alloc();
    match &input {
        Value::Table(t) => {
            if t.ncols() == 0 {
                return Value::string("table", alloc);
            }
            let mut out = AString::from_str_in("table<", alloc);
            for c in 0..t.ncols() {
                if c > 0 {
                    out.push_str(", ");
                }
                out.push_str(t.columns.get(c).unwrap().as_ref());
                out.push_str(": ");
                let ty = if t.nrows() > 0 {
                    t.cell(0, c).unwrap().type_name()
                } else {
                    "any"
                };
                out.push_str(ty);
            }
            out.push('>');
            Value::string(out.as_str(), alloc)
        }
        Value::Record(r) => {
            if r.is_empty() {
                return Value::string("record", alloc);
            }
            let mut out = AString::from_str_in("record<", alloc);
            for (i, (k, v)) in r.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(k);
                out.push_str(": ");
                out.push_str(v.type_name());
            }
            out.push('>');
            Value::string(out.as_str(), alloc)
        }
        other => Value::string(other.type_name(), alloc),
    }
}

fn builtin_echo<'a>(eval: &Evaluator<'a, '_>, args: EvalArgs<'a>) -> Value<'a> {
    let alloc = eval.alloc();
    match args.pos.len() {
        0 => Value::Nothing,
        1 => args.pos[0].0.share(),
        _ => {
            let mut list = PVec::new(alloc);
            for (v, _) in args.pos.iter() {
                list.push(v.share());
            }
            Value::List(list)
        }
    }
}

fn builtin_print<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let no_newline = args.has_flag("no-newline", "n");
    let mut text = AString::new_in(alloc);
    if args.pos.is_empty() {
        let _ = input.format_cell(&mut text);
    } else {
        for (i, (v, _)) in args.pos.iter().enumerate() {
            if i > 0 {
                text.push(' ');
            }
            let _ = v.format_cell(&mut text);
        }
    }
    eval.emit_stream(StreamChunk::Text { text, newline: !no_newline })?;
    Ok(Value::Nothing)
}

fn builtin_do<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    input: Value<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let Some((cval, cspan)) = args.pos.first() else {
        return Err(arity_err("do", "expected a closure", span, alloc));
    };
    let closure = require_closure(cval, "do", *cspan, alloc).map_err(Control::Error)?;
    let mut call_args = AVec::with_capacity_in(args.pos.len().saturating_sub(1), alloc);
    for (v, _) in args.pos.iter().skip(1) {
        call_args.push(v.share());
    }
    eval.invoke_closure(&closure, call_args.as_slice(), input, span)
}

fn builtin_help<'a>(
    eval: &Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    if args.pos.is_empty() {
        let mut table = Table::from_col_names(&["command", "signature", "summary"], alloc);
        for spec in BUILTIN_SPECS {
            let row = [
                Value::string(spec.name, alloc),
                Value::string(spec.signature, alloc),
                Value::string(spec.summary, alloc),
            ];
            table.push_row_values(&row);
        }
        return Ok(Value::Table(table));
    }

    let mut name = AString::new_in(alloc);
    for (i, (v, s)) in args.pos.iter().enumerate() {
        let w = require_string(v, "command", *s, alloc).map_err(Control::Error)?;
        if i > 0 {
            name.push(' ');
        }
        name.push_str(w.as_str());
    }

    if let Some(spec) = find_spec(name.as_str()) {
        let mut rec = Record::new(alloc);
        rec.insert_str("command", Value::string(spec.name, alloc));
        rec.insert_str("signature", Value::string(spec.signature, alloc));
        rec.insert_str("summary", Value::string(spec.summary, alloc));
        let mut flags = PVec::new(alloc);
        for &f in spec.flags {
            flags.push(Value::string(f, alloc));
        }
        rec.insert_str("flags", Value::List(flags));
        return Ok(Value::Record(rec));
    }

    if let Some(def) = eval.state().defs.get(name.as_str()) {
        let mut rec = Record::new(alloc);
        rec.insert_str("command", Value::String(def.name.share()));
        rec.insert_str("signature", Value::string("custom command", alloc));
        rec.insert_str("summary", Value::string("User-defined `def`", alloc));
        rec.insert_str("flags", Value::List(PVec::new(alloc)));
        return Ok(Value::Record(rec));
    }

    if let Some(exp) = eval.state().aliases.get(name.as_str()) {
        let mut rec = Record::new(alloc);
        rec.insert_str("command", Value::string(name.as_str(), alloc));
        rec.insert_str("signature", Value::String(exp.share()));
        rec.insert_str("summary", Value::string("User-defined `alias`", alloc));
        rec.insert_str("flags", Value::List(PVec::new(alloc)));
        return Ok(Value::Record(rec));
    }

    let mut msg = AString::from_str_in("no help found for `", alloc);
    msg.push_str(name.as_str());
    msg.push('`');
    Err(Control::Error(ShellError::new(ErrorKind::Undefined, span, msg.as_str(), alloc)))
}

fn builtin_fork<'a>(
    eval: &mut Evaluator<'a, '_>,
    args: EvalArgs<'a>,
    span: Span,
) -> Result<Value<'a>, Control<'a>> {
    let alloc = eval.alloc();
    let label = match args.pos.first() {
        Some((v, s)) => Some(require_string(v, "label", *s, alloc).map_err(Control::Error)?),
        None => None,
    };
    let ans = eval.request(Request::ForkSession { label }, span)?;
    match ans {
        Answer::Forked(branch) => Ok(Value::string(branch.as_str(), alloc)),
        _ => Err(Control::Error(ShellError::new(
            ErrorKind::Io,
            span,
            "unexpected host answer for `fork`",
            alloc,
        ))),
    }
}

/// Convert AST `PathStep` list to runtime `PathMember` list.
pub(crate) fn path_steps_to_members<'a>(
    steps: &[crate::ast::PathStep<'a>],
    alloc: Alloc<'a>,
) -> AVec<'a, PathMember<'a>> {
    let mut out = AVec::with_capacity_in(steps.len(), alloc);
    for st in steps {
        match &st.key {
            PathKey::Field(name) => {
                out.push(PathMember::Field { name: name.share(), optional: st.optional });
            }
            PathKey::Index(index) => {
                out.push(PathMember::Index { index: *index, optional: st.optional });
            }
        }
    }
    out
}
