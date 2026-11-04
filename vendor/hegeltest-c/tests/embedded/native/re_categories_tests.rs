use super::*;
use crate::native::re::constants::unicode_version_flags;
use crate::native::re::matcher::in_category;

const CLASSES: [ChCode; 3] = [ChCode::Digit, ChCode::Space, ChCode::Word];

fn python_class(c: char, class: ChCode, version: UnicodeVersion) -> bool {
    let category = version.general_category_char(c).as_str();
    let (major, minor) = (&category[..1], category);
    match class {
        ChCode::Digit => minor == "Nd",
        ChCode::Space => matches!(c, '\t'..='\r' | '\x1c'..=' ' | '\u{85}') || major == "Z",
        ChCode::Word => c == '_' || major == "L" || major == "N",
        _ => unreachable!(),
    }
}

#[test]
fn classes_follow_the_general_category() {
    for version in UnicodeVersion::all() {
        let flags = unicode_version_flags(version);
        let samples = (0..0x3000).chain((0x3000..=0x10FFFF).step_by(7));
        for cp in samples {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            for class in CLASSES {
                let expected = python_class(c, class, version);
                assert_eq!(
                    in_class(c, class, version),
                    expected,
                    "U+{cp:04X} {class:?}"
                );
                assert_eq!(
                    in_category(c, class, flags),
                    expected,
                    "U+{cp:04X} {class:?} {}",
                    version.as_str()
                );
                let ranges = class_ranges(class, version).unwrap();
                let in_ranges = ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&cp));
                assert_eq!(in_ranges, expected, "U+{cp:04X} {class:?}");
            }
        }
    }
}

#[test]
fn ranges_are_sorted_with_gaps_between() {
    for version in UnicodeVersion::all() {
        for class in CLASSES {
            let ranges = class_ranges(class, version).unwrap();
            assert!(!ranges.is_empty());
            for pair in ranges.windows(2) {
                assert!(
                    pair[0].0 <= pair[0].1 && pair[0].1 + 1 < pair[1].0,
                    "{pair:?}"
                );
            }
        }
    }
}

#[test]
fn classes_reach_beyond_the_basic_multilingual_plane() {
    let latest = UnicodeVersion::LATEST;
    assert!(in_class('\u{1D7CE}', ChCode::Digit, latest));
    assert!(in_class('\u{10400}', ChCode::Word, latest));
    assert!(!in_class('\u{1F600}', ChCode::Word, latest));
    assert!(!in_class('\u{10FFFF}', ChCode::Space, latest));
    assert!(in_class('\u{3000}', ChCode::Space, latest));
    assert!(!in_class('\u{85}', ChCode::Word, latest) && in_class('\u{85}', ChCode::Space, latest));
}

#[test]
fn classes_follow_the_version_assignments() {
    let v16 = UnicodeVersion::parse("16.0.0").unwrap();
    let v15_1 = UnicodeVersion::parse("15.1.0").unwrap();
    assert!(in_class('\u{A7CB}', ChCode::Word, v16));
    assert!(!in_class('\u{A7CB}', ChCode::Word, v15_1));
    assert!(in_class('\u{1CCF0}', ChCode::Digit, v16));
    assert!(!in_class('\u{1CCF0}', ChCode::Digit, v15_1));
    let word_16 = class_ranges(ChCode::Word, v16).unwrap();
    let word_15_1 = class_ranges(ChCode::Word, v15_1).unwrap();
    assert!(word_16.iter().any(|&(lo, hi)| (lo..=hi).contains(&0xA7CB)));
    assert!(
        !word_15_1
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&0xA7CB))
    );
}

#[test]
fn negated_classes_have_no_table() {
    for class in [ChCode::NotDigit, ChCode::NotSpace, ChCode::NotWord] {
        assert_eq!(class_ranges(class, UnicodeVersion::LATEST), None);
        assert!(!in_class('a', class, UnicodeVersion::LATEST));
    }
}
