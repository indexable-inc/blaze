//! Lists in English as rustc's diagnostics write them: "a", "a and b", then three forms of
//! three or more.

use std::fmt::{Display, Write};

/// How a list of three or more joins its last item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    /// "a, b, and c": `DiagArgValue::StrListSepByAnd`, `DiagSymbolList`.
    And,
    /// "a, b, or c": the parser's `tokens_to_string`, rustdoc's lists of choices.
    Or,
    /// "a, b and c": `rustc_lint_defs::listify` and the type checker's `listify`.
    AndNoSerialComma,
}

/// `items` joined as `join` says, each in backquotes when `quote`; "" when there are none.
pub fn list<I>(items: I, join: Join, quote: bool) -> String
where
    I: IntoIterator,
    I::IntoIter: ExactSizeIterator,
    I::Item: Display,
{
    let (pair, last) = match join {
        Join::And => (" and ", ", and "),
        Join::Or => (" or ", ", or "),
        Join::AndNoSerialComma => (" and ", " and "),
    };
    let quote = if quote { "`" } else { "" };
    let items = items.into_iter();
    let len = items.len();
    let mut out = String::new(); // alloc: a diagnostic's text, on its error path
    for (at, item) in items.enumerate() {
        let sep = match (at, len) {
            (0, _) => "",
            (1, 2) => pair,
            (at, _) if at + 1 == len => last,
            _ => ", ",
        };
        let _ = write!(out, "{sep}{quote}{item}{quote}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Join, list};

    /// The copies' form, a slice match with `join`: the reference.
    fn reference(items: &[String], join: Join) -> String {
        let (pair, last) = match join {
            Join::And => (" and ", ", and "),
            Join::Or => (" or ", ", or "),
            Join::AndNoSerialComma => (" and ", " and "),
        };
        match items {
            [] => String::new(),
            [one] => one.clone(),
            [a, b] => format!("{a}{pair}{b}"),
            [init @ .., end] => format!("{}{last}{end}", init.join(", ")),
        }
    }

    #[test]
    fn lists_are_joined_as_rustc_joins_them() {
        assert_eq!(list(["avx"], Join::And, false), "avx");
        assert_eq!(list(["avx", "bmi2"], Join::And, false), "avx and bmi2");
        assert_eq!(list(["avx", "bmi2", "fma"], Join::And, false), "avx, bmi2, and fma");
        assert_eq!(list(["a", "b", "c"], Join::Or, true), "`a`, `b`, or `c`");
        assert_eq!(list(["a", "b", "c"], Join::AndNoSerialComma, true), "`a`, `b` and `c`");
        assert_eq!(list(["a"], Join::AndNoSerialComma, true), "`a`");
        assert_eq!(list(Vec::<&str>::new(), Join::Or, true), "");
    }

    #[hegel::test]
    fn list_is_the_slice_matchs(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let items: Vec<String> = tc.draw(gs::vecs(gs::text().alphabet("ab ,").max_size(3)).max_size(5));
        let quote: bool = tc.draw(gs::booleans());
        for join in [Join::And, Join::Or, Join::AndNoSerialComma] {
            let quoted: Vec<String> =
                items.iter().map(|it| if quote { format!("`{it}`") } else { it.clone() }).collect();
            assert_eq!(list(&items, join, quote), reference(&quoted, join));
        }
    }
}
