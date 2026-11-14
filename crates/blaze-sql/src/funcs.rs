//! The functions of the subset and their types: one table that inference types a call by and
//! rendering writes a call by. A function not in [`FUNCTIONS`] is an error naming it.

#![blaze::sans_io]

use core::fmt;

/// How many arguments a function takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arity {
    Exactly(u32),
    AtLeast(u32),
}

impl Arity {
    /// Whether `count` arguments fit.
    pub fn admits(self, count: usize) -> bool {
        match self {
            Arity::Exactly(n) => usize::try_from(n).is_ok_and(|n| n == count),
            Arity::AtLeast(n) => usize::try_from(n).is_ok_and(|n| count >= n),
        }
    }
}

impl fmt::Display for Arity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arity::Exactly(1) => f.write_str("exactly 1 argument"),
            Arity::Exactly(n) => write!(f, "exactly {n} arguments"),
            Arity::AtLeast(n) => write!(f, "at least {n} arguments"),
        }
    }
}

/// What the arguments must be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Args {
    /// Any typed value (`count`). A bare parameter or `null` has no type here.
    Any,
    /// Any typed value but a Boolean (`min`, `max`).
    NotBoolean,
    /// An Integer or a Real (`sum`, `avg`, `abs`).
    Number,
    /// A Text; a parameter takes Text with the plain Rust type, non-null.
    Text,
    /// Values of one storage and one Rust type, which a parameter or `null` among them takes
    /// (`coalesce`).
    OneType,
}

/// What a call gives, from its argument's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gives {
    /// Integer, non-null (`count`).
    Count,
    /// The argument's storage with the plain Rust type, nullable: the sum of no rows is null
    /// (`sum`).
    StorageNullable,
    /// The argument's storage and Rust type, nullable (`min`, `max`).
    ArgumentNullable,
    /// Real, nullable (`avg`).
    RealNullable,
    /// The arguments' type, nullable only if every argument is (`coalesce`).
    FirstNonNull,
    /// Text with the plain Rust type, nullable as the argument (`lower`, `upper`).
    Text,
    /// Integer with the plain Rust type, nullable as the argument (`length`).
    Integer,
    /// The argument's storage with the plain Rust type, nullable as the argument (`abs`).
    Storage,
}

/// How Postgres's text of a call is made to give the type [`Gives`] says: Postgres's `sum` of a
/// `bigint` is a `numeric`, its `avg` a `numeric`, its `length` an `integer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Postgres {
    /// The call as written.
    AsIs,
    /// `cast(.. as bigint)`.
    Bigint,
    /// `cast(.. as double precision)`.
    DoublePrecision,
    /// `cast(.. as bigint)` when the argument is an Integer, as written when it is a Real.
    BigintOfInteger,
}

/// One function of the subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Function {
    /// Its name, lower case, as rendered. A call matches it without regard to ASCII case.
    pub name: &'static str,
    pub arity: Arity,
    pub args: Args,
    pub gives: Gives,
    /// An aggregate: reads a column over the rows of a group.
    pub aggregate: bool,
    /// Takes `*` as its argument (`count(*)`).
    pub takes_star: bool,
    pub postgres: Postgres,
}

/// The functions of the subset.
pub static FUNCTIONS: [Function; 10] = [
    Function {
        name: "count",
        arity: Arity::Exactly(1),
        args: Args::Any,
        gives: Gives::Count,
        aggregate: true,
        takes_star: true,
        postgres: Postgres::Bigint,
    },
    Function {
        name: "sum",
        arity: Arity::Exactly(1),
        args: Args::Number,
        gives: Gives::StorageNullable,
        aggregate: true,
        takes_star: false,
        postgres: Postgres::BigintOfInteger,
    },
    Function {
        name: "min",
        arity: Arity::Exactly(1),
        args: Args::NotBoolean,
        gives: Gives::ArgumentNullable,
        aggregate: true,
        takes_star: false,
        postgres: Postgres::AsIs,
    },
    Function {
        name: "max",
        arity: Arity::Exactly(1),
        args: Args::NotBoolean,
        gives: Gives::ArgumentNullable,
        aggregate: true,
        takes_star: false,
        postgres: Postgres::AsIs,
    },
    Function {
        name: "avg",
        arity: Arity::Exactly(1),
        args: Args::Number,
        gives: Gives::RealNullable,
        aggregate: true,
        takes_star: false,
        postgres: Postgres::DoublePrecision,
    },
    Function {
        name: "coalesce",
        arity: Arity::AtLeast(2),
        args: Args::OneType,
        gives: Gives::FirstNonNull,
        aggregate: false,
        takes_star: false,
        postgres: Postgres::AsIs,
    },
    Function {
        name: "lower",
        arity: Arity::Exactly(1),
        args: Args::Text,
        gives: Gives::Text,
        aggregate: false,
        takes_star: false,
        postgres: Postgres::AsIs,
    },
    Function {
        name: "upper",
        arity: Arity::Exactly(1),
        args: Args::Text,
        gives: Gives::Text,
        aggregate: false,
        takes_star: false,
        postgres: Postgres::AsIs,
    },
    Function {
        name: "length",
        arity: Arity::Exactly(1),
        args: Args::Text,
        gives: Gives::Integer,
        aggregate: false,
        takes_star: false,
        postgres: Postgres::Bigint,
    },
    Function {
        name: "abs",
        arity: Arity::Exactly(1),
        args: Args::Number,
        gives: Gives::Storage,
        aggregate: false,
        takes_star: false,
        postgres: Postgres::AsIs,
    },
];

/// The function a call names: an unquoted name without regard to ASCII case, a quoted one
/// exactly (so `"COUNT"` is no function, as on Postgres).
pub fn lookup(name: &str, quoted: bool) -> Option<&'static Function> {
    FUNCTIONS.iter().find(|function| {
        if quoted { function.name == name } else { function.name.eq_ignore_ascii_case(name) }
    })
}

/// The names of [`FUNCTIONS`], comma separated, for an error that lists them.
pub struct Known;

impl fmt::Display for Known {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, function) in FUNCTIONS.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(function.name)?;
        }
        Ok(())
    }
}
