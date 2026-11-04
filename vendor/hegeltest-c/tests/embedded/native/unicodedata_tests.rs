use super::*;
use alloc::string::ToString;
use alloc::vec;

type ReferenceRuns = &'static [(u32, u32, &'static str)];

const PYTHON_RANGES: &[(u32, &str)] = &include!("unicodedata_python_reference_data.rs");
const PYTHON_DELTAS: &[(&str, ReferenceRuns)] = &include!("unicodedata_python_reference_deltas.rs");

const LATEST: UnicodeVersion = UnicodeVersion::LATEST;

fn version(name: &str) -> UnicodeVersion {
    UnicodeVersion::parse(name).unwrap()
}

fn assert_matches(version: UnicodeVersion, expected: &[&str]) {
    for (cp, expected) in expected.iter().enumerate() {
        let got = version.general_category(cp as u32).unwrap().as_str();
        assert_eq!(
            got,
            *expected,
            "category mismatch at U+{cp:05X} in Unicode {}: got {got}, expected {expected}",
            version.as_str()
        );
    }
}

#[test]
fn matches_python_for_every_codepoint_of_every_version() {
    let mut expected: Vec<&str> = Vec::with_capacity(0x110000);
    for &(end, category) in PYTHON_RANGES {
        expected.resize(end as usize + 1, category);
    }
    assert_eq!(
        expected.len(),
        0x110000,
        "python reference does not cover full range"
    );
    assert_matches(LATEST, &expected);

    let older: Vec<&str> = PYTHON_DELTAS.iter().map(|(name, _)| *name).collect();
    let versions: Vec<&str> = UnicodeVersion::all()
        .skip(1)
        .map(UnicodeVersion::as_str)
        .collect();
    assert_eq!(older, versions);
    for &(name, runs) in PYTHON_DELTAS {
        for &(start, end, category) in runs {
            for slot in &mut expected[start as usize..=end as usize] {
                *slot = category;
            }
        }
        assert_matches(version(name), &expected);
    }
}

#[test]
fn versions_parse_by_number() {
    assert_eq!(version("17.0.0"), LATEST);
    assert_eq!(LATEST.as_str(), "17.0.0");
    assert_eq!(version("16.0.0").as_str(), "16.0.0");
    assert_eq!(version("15.1.0").as_str(), "15.1.0");
    assert_eq!(version("15.1").as_str(), "15.1.0");
    assert_eq!(version("15").as_str(), "15.0.0");
    assert_eq!(version("14.0.0").as_str(), "14.0.0");
    assert_eq!(version("18.0.0"), LATEST);
    assert_eq!(version("17.0.1"), LATEST);
    assert_eq!(version("99.9"), LATEST);
    for name in ["13.0.0", "15.0.1", "15.2", "0.0.0"] {
        let err = UnicodeVersion::parse(name).unwrap_err().to_string();
        assert!(matches!(
            UnicodeVersion::parse(name),
            Err(EngineError::InvalidArgument(_))
        ));
        assert!(
            err.contains(&format!("no tables for Unicode {name}"))
                && err.contains("14.0.0, 15.0.0, 15.1.0, 16.0.0, 17.0.0"),
            "{err}"
        );
    }
    for name in ["", "x", "15.1.0.0", "15.a", "1 5", "-15.0.0", "15..0"] {
        let err = UnicodeVersion::parse(name).unwrap_err().to_string();
        assert!(
            err.contains("is not a Unicode version number"),
            "{name:?}: {err}"
        );
    }
}

#[test]
fn per_version_cells_compute_once_and_independently() {
    let cells: PerVersion<u32> = PerVersion::new();
    let v16 = version("16.0.0");
    assert_eq!(*cells.get(LATEST, || 17), 17);
    assert_eq!(*cells.get(LATEST, || 99), 17);
    assert_eq!(*cells.get(v16, || 16), 16);
    assert_eq!(*cells.get(LATEST, || 99), 17);
    for v in UnicodeVersion::all() {
        assert_eq!(
            *cells.get(v, || v.index() + 100),
            if v == LATEST {
                17
            } else if v == v16 {
                16
            } else {
                v.index() + 100
            }
        );
    }
}

#[test]
fn version_indices_round_trip() {
    let all: Vec<UnicodeVersion> = UnicodeVersion::all().collect();
    assert_eq!(all.len(), 5);
    assert_eq!(all[0], LATEST);
    for (i, &v) in all.iter().enumerate() {
        assert_eq!(v.index(), i as u32);
        assert_eq!(UnicodeVersion::from_index(i as u32), v);
    }
    assert_eq!(UnicodeVersion::from_index(99), all[4]);
    assert_eq!(all[4].as_str(), "14.0.0");
}

#[test]
fn older_versions_follow_their_own_assignments() {
    let v16 = version("16.0.0");
    let v15_1 = version("15.1.0");
    assert_eq!(LATEST.general_category(0x295).unwrap(), Category::Lo);
    assert_eq!(v16.general_category(0x295).unwrap(), Category::Ll);
    assert_eq!(LATEST.general_category(0x323B0).unwrap(), Category::Lo);
    assert_eq!(v16.general_category(0x323B0).unwrap(), Category::Cn);
    assert_eq!(v16.general_category(0xA7CB).unwrap(), Category::Lu);
    assert_eq!(v15_1.general_category(0xA7CB).unwrap(), Category::Cn);
    assert_eq!(LATEST.to_upper('\u{A7CF}'), '\u{A7CE}');
    assert_eq!(v16.to_upper('\u{A7CF}'), '\u{A7CF}');
    assert_eq!(v16.to_upper('\u{1C8A}'), '\u{1C89}');
    assert_eq!(v15_1.to_upper('\u{1C8A}'), '\u{1C8A}');
    assert_eq!(v15_1.to_lower('\u{1C89}'), '\u{1C89}');
    assert_eq!(v15_1.to_lower('A'), 'a');
    assert!(v15_1.is_in_group(0x41, "Lu").unwrap());
    assert!(!v15_1.is_in_group(0xA7CB, "L").unwrap());
}

#[test]
fn case_tables_are_sorted_and_cased_in_their_version() {
    for version in UnicodeVersion::all() {
        let mappings = version.case_mappings();
        assert!(!mappings.is_empty());
        for pair in mappings.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{pair:?} in {}", version.as_str());
        }
        for &(cp, lower, upper) in mappings {
            assert!(
                lower != cp || upper != cp,
                "U+{cp:04X} in {}",
                version.as_str()
            );
            for c in [cp, lower, upper] {
                assert_ne!(
                    version.general_category(c).unwrap(),
                    Category::Cn,
                    "U+{c:04X} in {}",
                    version.as_str()
                );
            }
        }
    }
}

fn brute_force_overlay(
    base: &[(u32, Category)],
    runs: &[(u32, u32, Category)],
    top: u32,
) -> Vec<(u32, Category)> {
    let mut out: Vec<(u32, Category)> = Vec::new();
    for cp in 0..=top {
        let idx = base.partition_point(|&(end, _)| end < cp);
        let mut cat = base[idx].1;
        if let Some(&(_, _, c)) = runs.iter().find(|&&(s, e, _)| (s..=e).contains(&cp)) {
            cat = c;
        }
        match out.last_mut() {
            Some(last) if last.1 == cat => last.0 = cp,
            _ => out.push((cp, cat)),
        }
    }
    out
}

#[test]
fn overlaying_runs_matches_a_per_codepoint_overlay() {
    use Category::*;
    let base = vec![(4, Cc), (9, Lu), (19, Ll), (29, Cn), (39, Lo)];
    let cases: Vec<Vec<(u32, u32, Category)>> = vec![
        vec![],
        vec![(0, 2, Lu)],
        vec![(0, 4, Lu)],
        vec![(5, 9, Cc)],
        vec![(8, 12, Nd)],
        vec![(8, 25, Nd)],
        vec![(10, 19, Cn)],
        vec![(20, 24, Lo), (25, 29, Lo)],
        vec![(20, 29, Lo)],
        vec![(35, 39, Cn)],
        vec![(39, 39, Cn)],
        vec![(0, 39, So)],
        vec![
            (2, 3, So),
            (6, 6, So),
            (12, 12, Lu),
            (27, 33, Zs),
            (38, 39, Zs),
        ],
    ];
    for runs in cases {
        let overlaid = overlay_runs(&base, &runs);
        assert_eq!(overlaid, brute_force_overlay(&base, &runs, 39), "{runs:?}");
        assert_eq!(overlaid.last().unwrap().0, 39);
        for pair in overlaid.windows(2) {
            assert!(
                pair[0].0 < pair[1].0 && pair[0].1 != pair[1].1,
                "{overlaid:?}"
            );
        }
    }
}

#[test]
fn overlaying_case_changes_inserts_replaces_and_removes() {
    let base = vec![
        (0x41, 0x61, 0x41),
        (0x61, 0x61, 0x41),
        (0x100, 0x101, 0x100),
    ];
    assert_eq!(overlay_case(&base, &[]), base);
    assert_eq!(
        overlay_case(
            &base,
            &[
                (0x30, 0x30, 0x31),
                (0x61, 0x61, 0x61),
                (0x80, 0x81, 0x80),
                (0x100, 0x101, 0x1FF),
                (0x200, 0x201, 0x200),
            ]
        ),
        vec![
            (0x30, 0x30, 0x31),
            (0x41, 0x61, 0x41),
            (0x80, 0x81, 0x80),
            (0x100, 0x101, 0x1FF),
            (0x200, 0x201, 0x200),
        ]
    );
    assert_eq!(
        overlay_case(
            &base,
            &[
                (0x41, 0x41, 0x41),
                (0x61, 0x61, 0x61),
                (0x100, 0x100, 0x100)
            ]
        ),
        vec![]
    );
}

#[test]
fn version_numbers_parse_leniently_but_not_loosely() {
    assert_eq!(version_number("15.1.0"), Some((15, 1, 0)));
    assert_eq!(version_number("15.1"), Some((15, 1, 0)));
    assert_eq!(version_number("15"), Some((15, 0, 0)));
    assert_eq!(version_number("15.1.0.0"), None);
    assert_eq!(version_number(""), None);
    assert_eq!(version_number("15."), None);
    assert_eq!(version_number(".1"), None);
    assert_eq!(version_number("v15"), None);
}

#[test]
fn category_as_str_is_two_chars() {
    use Category::*;
    for cat in [
        Lu, Ll, Lt, Lm, Lo, Mn, Mc, Me, Nd, Nl, No, Pc, Pd, Ps, Pe, Pi, Pf, Po, Sm, Sc, Sk, So, Zs,
        Zl, Zp, Cc, Cf, Cs, Co, Cn,
    ] {
        assert_eq!(cat.as_str().len(), 2);
    }
}

#[test]
fn edge_codepoints() {
    assert_eq!(LATEST.general_category(0x0000).unwrap(), Category::Cc);
    assert_eq!(LATEST.general_category(0x0020).unwrap(), Category::Zs);
    assert_eq!(LATEST.general_category(0x0030).unwrap(), Category::Nd);
    assert_eq!(LATEST.general_category(0x005F).unwrap(), Category::Pc);
    assert_eq!(LATEST.general_category(0x002D).unwrap(), Category::Pd);
    assert_eq!(LATEST.general_category(0x002B).unwrap(), Category::Sm);
    assert_eq!(LATEST.general_category(0xD800).unwrap(), Category::Cs);
    assert_eq!(LATEST.general_category(0xDFFF).unwrap(), Category::Cs);
    assert_eq!(LATEST.general_category(0xE000).unwrap(), Category::Co);
    assert_eq!(LATEST.general_category(0xFDD0).unwrap(), Category::Cn);
    assert_eq!(LATEST.general_category(0x10FFFF).unwrap(), Category::Cn);
}

#[test]
fn beyond_max_codepoint_is_an_internal_error() {
    let msg = LATEST.general_category(0x110000).unwrap_err().to_string();
    assert!(msg.contains("out of range"), "{msg}");
    assert!(msg.contains("bug in hegel"), "{msg}");
}

#[test]
fn char_variants_agree_with_the_u32_lookups() {
    assert_eq!(LATEST.general_category_char('A'), Category::Lu);
    assert_eq!(
        LATEST.general_category_char('\u{10FFFF}'),
        LATEST.general_category(0x10FFFF).unwrap()
    );
    assert!(LATEST.is_in_group('A' as u32, "Lu").unwrap());
    assert!(LATEST.is_in_group('A' as u32, "L").unwrap());
    assert!(!LATEST.is_in_group('A' as u32, "N").unwrap());
}

#[test]
fn is_in_group_two_char_matches_exactly() {
    assert!(LATEST.is_in_group('A' as u32, "Lu").unwrap());
    assert!(!LATEST.is_in_group('A' as u32, "Ll").unwrap());
    assert!(LATEST.is_in_group('a' as u32, "Ll").unwrap());
    assert!(LATEST.is_in_group('_' as u32, "Pc").unwrap());
    assert!(!LATEST.is_in_group('_' as u32, "Po").unwrap());
}

#[test]
fn is_in_group_major_class_matches_all_subclasses() {
    for &cp in &['A' as u32, 'a' as u32, 0x01C5, 0x02B0, 0x00AA] {
        assert!(
            LATEST.is_in_group(cp, "L").unwrap(),
            "U+{cp:04X} should match L"
        );
    }
    assert!(!LATEST.is_in_group(' ' as u32, "L").unwrap());
    assert!(!LATEST.is_in_group('0' as u32, "L").unwrap());

    assert!(LATEST.is_in_group('0' as u32, "N").unwrap());
    assert!(LATEST.is_in_group(0x2160, "N").unwrap());
    assert!(!LATEST.is_in_group('A' as u32, "N").unwrap());

    for &cp in &[
        '_' as u32, '-' as u32, '(' as u32, ')' as u32, 0x00AB, 0x00BB, '.' as u32,
    ] {
        assert!(
            LATEST.is_in_group(cp, "P").unwrap(),
            "U+{cp:04X} should match P"
        );
    }

    assert!(LATEST.is_in_group(' ' as u32, "Z").unwrap());
    assert!(LATEST.is_in_group(0x2028, "Z").unwrap());
    assert!(LATEST.is_in_group(0x2029, "Z").unwrap());
}

#[test]
fn is_in_group_unknown_or_invalid_matches_nothing() {
    assert!(!LATEST.is_in_group('A' as u32, "Xx").unwrap());
    assert!(!LATEST.is_in_group('A' as u32, "X").unwrap());
    assert!(!LATEST.is_in_group('A' as u32, "").unwrap());
    assert!(!LATEST.is_in_group('A' as u32, "Lux").unwrap());
    assert!(!Category::Lu.in_group("l"));
    assert!(Category::Lu.in_group("L"));
}

#[test]
fn nfd_base_decomposes_diacritic_letters_to_ascii() {
    for cp in [0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5] {
        assert_eq!(nfd_base(cp), Some('A' as u32), "U+{cp:04X} → A");
    }
    for cp in [0x00E0, 0x00E1, 0x00E2, 0x00E3, 0x00E4, 0x00E5] {
        assert_eq!(nfd_base(cp), Some('a' as u32), "U+{cp:04X} → a");
    }
    assert_eq!(nfd_base(0x00D1), Some('N' as u32));
    assert_eq!(nfd_base(0x00F1), Some('n' as u32));
}

#[test]
fn nfd_base_resolves_recursively() {
    assert_eq!(nfd_base(0x01FA), Some('A' as u32));
    assert_eq!(nfd_base(0x00C5), Some('A' as u32));
}

#[test]
fn nfd_base_returns_none_for_non_decomposable() {
    assert_eq!(nfd_base('A' as u32), None);
    assert_eq!(nfd_base('0' as u32), None);
    assert_eq!(nfd_base(0x00DF), None);
    assert_eq!(nfd_base(0x2160), None);
    assert_eq!(nfd_base(0x1F600), None);
    assert_eq!(nfd_base(0x82535), None);
}

#[test]
fn category_from_code_round_trips_every_variant() {
    let all = [
        Category::Lu,
        Category::Ll,
        Category::Lt,
        Category::Lm,
        Category::Lo,
        Category::Mn,
        Category::Mc,
        Category::Me,
        Category::Nd,
        Category::Nl,
        Category::No,
        Category::Pc,
        Category::Pd,
        Category::Ps,
        Category::Pe,
        Category::Pi,
        Category::Pf,
        Category::Po,
        Category::Sm,
        Category::Sc,
        Category::Sk,
        Category::So,
        Category::Zs,
        Category::Zl,
        Category::Zp,
        Category::Cc,
        Category::Cf,
        Category::Cs,
        Category::Co,
        Category::Cn,
    ];
    for cat in all {
        assert_eq!(Category::from_code(cat.as_str()), Some(cat));
    }
}

#[test]
fn category_from_code_rejects_unknown() {
    assert_eq!(Category::from_code(""), None);
    assert_eq!(Category::from_code("Xx"), None);
    assert_eq!(Category::from_code("lu"), None);
    assert_eq!(Category::from_code("Luu"), None);
}

#[test]
fn category_runs_tile_the_codespace() {
    for version in UnicodeVersion::all() {
        let mut next = 0;
        for (start, end, cat) in version.category_runs() {
            assert_eq!(start, next);
            assert!(start <= end);
            assert_eq!(version.general_category(start).unwrap(), cat);
            assert_eq!(version.general_category(end).unwrap(), cat);
            next = end + 1;
        }
        assert_eq!(next, 0x10FFFF + 1);
    }
}

#[test]
fn category_runs_in_clip_the_runs_to_the_range() {
    for &(lo, hi) in &[
        (0, 0xFFFF),
        (0x41, 0x5A),
        (0x30, 0x7F),
        (0x3A, 0x3A),
        (0x10FFFF, 0x10FFFF),
        (0x10FF00, 0x11FFFF),
        (0, 0x10FFFF),
    ] {
        let top = hi.min(0x10FFFF);
        let mut next = lo;
        for (start, end, cat) in LATEST.category_runs_in(lo, hi) {
            assert_eq!(start, next);
            assert!(start <= end && end <= top);
            assert_eq!(LATEST.general_category(start).unwrap(), cat);
            assert_eq!(LATEST.general_category(end).unwrap(), cat);
            next = end + 1;
        }
        assert_eq!(next, top + 1, "runs over {lo:#x}..={hi:#x}");
    }
    assert!(
        LATEST
            .category_runs_in(0, 0x10FFFF)
            .eq(LATEST.category_runs())
    );
    assert_eq!(
        LATEST.category_runs_in(0x41, 0x5A).collect::<Vec<_>>(),
        vec![(0x41, 0x5A, Category::Lu)]
    );
    assert!(LATEST.category_runs_in(0x30, 0x7F).count() > 1);
    assert_eq!(LATEST.category_runs_in(5, 3).count(), 0);
    assert_eq!(LATEST.category_runs_in(0x110000, 0x110005).count(), 0);
}

/// Expected values from CPython 3.14's `_sre.unicode_tolower` and
/// `_sre.unicode_iscased`, which the table derivation was checked against
/// for every codepoint of Unicode 16.0.
#[test]
fn case_mappings_follow_cpython() {
    assert_eq!(LATEST.to_lower('A'), 'a');
    assert_eq!(LATEST.to_upper('a'), 'A');
    assert_eq!(LATEST.to_lower('\u{130}'), 'i');
    assert_eq!(LATEST.to_upper('\u{130}'), '\u{130}');
    assert_eq!(LATEST.to_lower('\u{DF}'), '\u{DF}');
    assert_eq!(LATEST.to_upper('\u{DF}'), 'S');
    assert_eq!(LATEST.to_upper('\u{149}'), '\u{2BC}');
    assert_eq!(LATEST.to_lower('\u{212A}'), 'k');
    assert_eq!(LATEST.to_upper('\u{1C8A}'), '\u{1C89}');
    assert_eq!(LATEST.to_lower('1'), '1');
    assert_eq!(LATEST.to_upper('1'), '1');
    assert_eq!(LATEST.to_lower('\u{4E00}'), '\u{4E00}');
    assert_eq!(LATEST.to_lower('\u{10FFFF}'), '\u{10FFFF}');
}

#[test]
fn unicode_17_case_mappings_are_known() {
    assert_eq!(LATEST.to_upper('\u{A7CF}'), '\u{A7CE}');
    assert_eq!(LATEST.to_lower('\u{A7CE}'), '\u{A7CF}');
    assert_eq!(LATEST.general_category(0x105C0).unwrap(), Category::Lo);
}

#[test]
#[should_panic(expected = "malformed delta line")]
fn a_delta_line_of_unknown_kind_is_malformed() {
    Tables::latest().with_delta("X 1 2 3");
}

#[test]
fn the_host_version_is_the_latest_until_set() {
    assert_eq!(UnicodeVersion::host(), LATEST);
    UnicodeVersion::set_host(version("15.1.0"));
    assert_eq!(UnicodeVersion::host(), version("15.1.0"));
    UnicodeVersion::set_host(LATEST);
    assert_eq!(UnicodeVersion::host(), LATEST);
}
