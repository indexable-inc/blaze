//! Functions for string case manipulation, such as detecting the identifier case,
//! and converting it into appropriate form.

// Code that was taken from rustc was taken at commit 89fdb30,
// from file /compiler/rustc_lint/src/nonstandard_style.rs

/// Converts an identifier to an UpperCamelCase form.
/// Returns `None` if the string is already in UpperCamelCase.
pub(crate) fn to_camel_case(ident: &str) -> Option<String> {
    if is_camel_case(ident) {
        return None;
    }

    Some(stdx::to_camel_case(ident))
}

/// Converts an identifier to a lower_snake_case form.
/// Returns `None` if the string is already in lower_snake_case.
pub(crate) fn to_lower_snake_case(ident: &str) -> Option<String> {
    if is_lower_snake_case(ident) {
        return None;
    } else if is_upper_snake_case(ident) {
        return Some(ident.to_lowercase());
    }

    Some(stdx::to_lower_snake_case(ident))
}

/// Converts an identifier to an UPPER_SNAKE_CASE form.
/// Returns `None` if the string is already is UPPER_SNAKE_CASE.
pub(crate) fn to_upper_snake_case(ident: &str) -> Option<String> {
    if is_upper_snake_case(ident) {
        return None;
    } else if is_lower_snake_case(ident) {
        return Some(ident.to_uppercase());
    }

    Some(stdx::to_upper_snake_case(ident))
}

// Taken from rustc's `is_upper_camel_case`.
fn is_camel_case(name: &str) -> bool {
    let name = name.trim_matches('_');
    let Some(first) = name.chars().next() else {
        return true;
    };
    // some scripts don't have a concept of upper/lowercase
    !(changes_when_titlecased(first) || not_camel_case(name))
}

// contains a capitalisable character followed by, or preceded by, an underscore,
// or contains an uppercase character that changes when titlecased,
// or contains `__`
fn not_camel_case(s: &str) -> bool {
    let mut last = '\0';
    s.chars().any(|snd| {
        let fst = std::mem::replace(&mut last, snd);
        match (fst, snd) {
            ('_', '_') => true,
            ('_', _) if char_has_case(snd) => true,
            (_, '_') if char_has_case(fst) => true,
            _ => snd.is_uppercase() && changes_when_titlecased(snd),
        }
    })
}

/// Some characters have a case but cannot be titlecased or lowercased: rustc's lints care
/// about whether the case can change.
fn char_has_case(c: char) -> bool {
    !c.to_lowercase().eq(to_titlecase(c))
}

fn changes_when_titlecased(c: char) -> bool {
    !to_titlecase(c).eq([c])
}

/// `char::to_titlecase`, which is unstable: the uppercase mapping but where the titlecase
/// one of the Unicode Character Database differs, as core's `TITLECASE_LUT` has them.
fn to_titlecase(c: char) -> impl Iterator<Item = char> {
    let to = |code: u32| [char::from_u32(code).unwrap_or(c), '\0', '\0'];
    let code = c as u32;
    let mapped = match code {
        0x01c4 | 0x01c7 | 0x01ca | 0x01f1 => to(code + 1),
        0x01c6 | 0x01c9 | 0x01cc | 0x01f3 => to(code - 1),
        0x01c5 | 0x01c8 | 0x01cb | 0x01f2 | 0x10d0..=0x10fa | 0x10fd..=0x10ff => to(code),
        0x1f88..=0x1f8f | 0x1f98..=0x1f9f | 0x1fa8..=0x1faf | 0x1fbc | 0x1fcc | 0x1ffc => to(code),
        0x1f80..=0x1f87 | 0x1f90..=0x1f97 | 0x1fa0..=0x1fa7 => to(code + 8),
        0x1fb3 | 0x1fc3 | 0x1ff3 => to(code + 9),
        0x00df => ['S', 's', '\0'],
        0x0587 => ['\u{535}', '\u{582}', '\0'],
        0x1fb2 => ['\u{1fba}', '\u{345}', '\0'],
        0x1fb4 => ['\u{386}', '\u{345}', '\0'],
        0x1fb7 => ['\u{391}', '\u{342}', '\u{345}'],
        0x1fc2 => ['\u{1fca}', '\u{345}', '\0'],
        0x1fc4 => ['\u{389}', '\u{345}', '\0'],
        0x1fc7 => ['\u{397}', '\u{342}', '\u{345}'],
        0x1ff2 => ['\u{1ffa}', '\u{345}', '\0'],
        0x1ff4 => ['\u{38f}', '\u{345}', '\0'],
        0x1ff7 => ['\u{3a9}', '\u{342}', '\u{345}'],
        0xfb00 => ['F', 'f', '\0'],
        0xfb01 => ['F', 'i', '\0'],
        0xfb02 => ['F', 'l', '\0'],
        0xfb03 => ['F', 'f', 'i'],
        0xfb04 => ['F', 'f', 'l'],
        0xfb05 | 0xfb06 => ['S', 't', '\0'],
        0xfb13 => ['\u{544}', '\u{576}', '\0'],
        0xfb14 => ['\u{544}', '\u{565}', '\0'],
        0xfb15 => ['\u{544}', '\u{56b}', '\0'],
        0xfb16 => ['\u{54e}', '\u{576}', '\0'],
        0xfb17 => ['\u{544}', '\u{56d}', '\0'],
        _ => {
            let mut upper = c.to_uppercase();
            [c, '\0', '\0'].map(|_| upper.next().unwrap_or('\0'))
        }
    };
    let len = 1 + mapped[1..].iter().take_while(|&&it| it != '\0').count();
    mapped.into_iter().take(len)
}

// Taken from rustc's `NonSnakeCase::check_snake_case`.
fn is_lower_snake_case(ident: &str) -> bool {
    let ident = ident.trim_start_matches('\'').trim_matches('_');
    !ident.contains("__") && ident.chars().all(|c| c.to_lowercase().eq([c]))
}

// Taken from rustc's `NonUpperCaseGlobals::check_upper_case`.
fn is_upper_snake_case(ident: &str) -> bool {
    ident.chars().all(|c| c.to_uppercase().eq([c]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::{Expect, expect};

    fn check<F: Fn(&str) -> Option<String>>(fun: F, input: &str, expect: Expect) {
        // `None` is translated to empty string, meaning that there is nothing to fix.
        let output = fun(input).unwrap_or_default();

        expect.assert_eq(&output);
    }

    #[test]
    fn test_to_lower_snake_case() {
        check(to_lower_snake_case, "lower_snake_case", expect![[""]]);
        check(to_lower_snake_case, "UPPER_SNAKE_CASE", expect![["upper_snake_case"]]);
        check(to_lower_snake_case, "Weird_Case", expect![["weird_case"]]);
        check(to_lower_snake_case, "UpperCamelCase", expect![["upper_camel_case"]]);
        check(to_lower_snake_case, "lowerCamelCase", expect![["lower_camel_case"]]);
        check(to_lower_snake_case, "a", expect![[""]]);
        check(to_lower_snake_case, "abc", expect![[""]]);
        check(to_lower_snake_case, "foo__bar", expect![["foo_bar"]]);
        check(to_lower_snake_case, "Δ", expect!["δ"]);
    }

    #[test]
    fn test_to_camel_case() {
        check(to_camel_case, "UpperCamelCase", expect![[""]]);
        check(to_camel_case, "UpperCamelCase_", expect![[""]]);
        check(to_camel_case, "_CamelCase", expect![[""]]);
        check(to_camel_case, "lowerCamelCase", expect![["LowerCamelCase"]]);
        check(to_camel_case, "lower_snake_case", expect![["LowerSnakeCase"]]);
        check(to_camel_case, "UPPER_SNAKE_CASE", expect![["UpperSnakeCase"]]);
        check(to_camel_case, "Weird_Case", expect![["WeirdCase"]]);
        check(to_camel_case, "name", expect![["Name"]]);
        check(to_camel_case, "A", expect![[""]]);
        check(to_camel_case, "AABB", expect![[""]]);
        // Taken from rustc: /compiler/rustc_lint/src/nonstandard_style/tests.rs
        check(to_camel_case, "X86_64", expect![[""]]);
        check(to_camel_case, "x86__64", expect![["X86_64"]]);
        check(to_camel_case, "Abc_123", expect![["Abc123"]]);
        check(to_camel_case, "A1_b2_c3", expect![["A1B2C3"]]);
    }

    #[test]
    fn test_to_upper_snake_case() {
        check(to_upper_snake_case, "UPPER_SNAKE_CASE", expect![[""]]);
        check(to_upper_snake_case, "lower_snake_case", expect![["LOWER_SNAKE_CASE"]]);
        check(to_upper_snake_case, "Weird_Case", expect![["WEIRD_CASE"]]);
        check(to_upper_snake_case, "UpperCamelCase", expect![["UPPER_CAMEL_CASE"]]);
        check(to_upper_snake_case, "lowerCamelCase", expect![["LOWER_CAMEL_CASE"]]);
        check(to_upper_snake_case, "A", expect![[""]]);
        check(to_upper_snake_case, "ABC", expect![[""]]);
        check(to_upper_snake_case, "X86_64", expect![[""]]);
        check(to_upper_snake_case, "FOO_BAr", expect![["FOO_BAR"]]);
        // rustc's says nothing of underscores in globals.
        check(to_upper_snake_case, "FOO__BAR", expect![[""]]);
        check(to_upper_snake_case, "ß", expect!["SS"]);
        check(to_upper_snake_case, "__密__封__", expect![[""]]);
    }

    #[test]
    fn caseless_and_titlecase_letters_are_as_rustc_says() {
        // Mkhedruli titlecases to itself, so rustc takes it for a script without case.
        check(to_camel_case, "ძალა_ერთობაშია", expect![[""]]);
        check(to_camel_case, "你_好", expect![[""]]);
        // A titlecase digraph lowercases, so it is not snake case.
        assert!(to_lower_snake_case("ǅ").is_some());
        assert!(to_upper_snake_case("ǅ").is_some());
        assert!(to_camel_case("Ǆa").is_some());
    }
}
