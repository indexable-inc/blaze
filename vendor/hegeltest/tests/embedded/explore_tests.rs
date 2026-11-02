//! Embedded tests for `src/explore.rs`.

use super::*;

#[test]
fn integers_convert_within_their_range() {
    assert_eq!(Integer::from(-5i64).to_i128(), Some(-5));
    assert_eq!(Integer::from(-5i64).to_u128(), None);
    assert_eq!(Integer::from(u128::MAX).to_i128(), None);
    assert_eq!(Integer::from(u128::MAX).to_u128(), Some(u128::MAX));
    assert_eq!(Integer::from(i128::MIN).to_i128(), Some(i128::MIN));
    assert_eq!(Integer::from(0i8), Integer::from(0u8));
    assert_eq!(Integer::from(-42i32).to_string(), "-42");
    assert_eq!(Integer::from(42usize).to_string(), "42");
}

#[test]
fn integers_order_numerically() {
    let mut values: Vec<Integer> = [3i128, -1, 0, -7, 2]
        .into_iter()
        .map(Integer::from)
        .collect();
    values.push(Integer::from(u128::MAX));
    values.sort();
    let shown: Vec<String> = values.iter().map(Integer::to_string).collect();
    assert_eq!(shown, ["-7", "-1", "0", "2", "3", &u128::MAX.to_string()]);
    assert!(
        Integer::from(-1i8).partial_cmp(&Integer::from(1u8)) == Some(core::cmp::Ordering::Less)
    );
}

#[test]
fn integer_bytes_round_trip() {
    for value in [
        Integer::from(0u8),
        Integer::from(1u8),
        Integer::from(-1i8),
        Integer::from(i128::MIN),
        Integer::from(u128::MAX),
        Integer::from_magnitude(true, u128::MAX),
    ] {
        assert_eq!(Integer::from_le_bytes(&value.to_le_bytes()), Some(value));
    }
}

#[test]
fn integer_bytes_accept_any_two_s_complement_width() {
    assert_eq!(Integer::from_le_bytes(&[]), Some(Integer::from(0u8)));
    assert_eq!(Integer::from_le_bytes(&[0xff]), Some(Integer::from(-1i8)));
    assert_eq!(
        Integer::from_le_bytes(
            &[0xfe; 20]
                .iter()
                .enumerate()
                .map(|(i, &b)| if i == 0 { b } else { 0xff })
                .collect::<Vec<_>>()
        ),
        Some(Integer::from(-2i8))
    );
    assert_eq!(
        Integer::from_le_bytes(&[5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Some(Integer::from(5u8))
    );
}

#[test]
fn integer_bytes_outside_the_range_are_refused() {
    let mut too_big = [0u8; 18];
    too_big[17] = 1;
    assert_eq!(Integer::from_le_bytes(&too_big), None);
    let mut too_big = [0u8; 17];
    too_big[16] = 1;
    assert_eq!(Integer::from_le_bytes(&too_big), None);
    let mut too_small = [0u8; 17];
    too_small[16] = 0xff;
    assert_eq!(Integer::from_le_bytes(&too_small), None);
    let mut too_small = [0xffu8; 17];
    too_small[16] = 0x80;
    assert_eq!(Integer::from_le_bytes(&too_small), None);
}

fn report_bytes(body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = vec![REPORT_VERSION];
    out.extend_from_slice(&7u64.to_le_bytes());
    body(&mut out);
    out
}

fn empty_tail(out: &mut Vec<u8>) {
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

#[test]
fn a_minimal_report_decodes() {
    let bytes = report_bytes(|out| {
        out.extend_from_slice(&[1, 0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        empty_tail(out);
    });
    let report = decode_report(&bytes);
    assert_eq!(report.id, 7);
    assert_eq!(report.status, Status::Invalid);
    assert!(report.choices().is_empty());
}

#[test]
fn malformed_reports_are_refused() {
    let unknown_status = report_bytes(|out| out.push(9));
    let bad_bool = report_bytes(|out| out.extend_from_slice(&[0, 2]));
    let unknown_tag = report_bytes(|out| {
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&[9, 0]);
    });
    let bad_utf8 = report_bytes(|out| {
        out.extend_from_slice(&[3, 1]);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.push(0xff);
    });
    let trailing = report_bytes(|out| {
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        empty_tail(out);
        out.push(0);
    });
    for bytes in [
        vec![],
        vec![REPORT_VERSION + 1],
        report_bytes(|_| {}),
        unknown_status,
        bad_bool,
        unknown_tag,
        bad_utf8,
        trailing,
    ] {
        assert_eq!(parse_report(&bytes), None, "{bytes:?}");
    }
}

#[test]
fn a_malformed_report_is_an_internal_error() {
    let panic = std::panic::catch_unwind(|| decode_report(&[])).unwrap_err();
    let msg = panic.downcast_ref::<String>().unwrap();
    assert!(msg.contains("malformed case report"), "{msg}");
}

#[test]
fn a_size_past_usize_is_refused() {
    let mut r = Reader {
        bytes: &u64::MAX.to_le_bytes(),
        pos: 0,
    };
    let fits = usize::try_from(u64::MAX).is_ok();
    assert_eq!(r.size().is_some(), fits);
}

#[test]
fn a_clone_choice_encodes_its_children_as_a_nested_list() {
    let bytes = encode_choices(&[Choice::Clone(vec![Choice::Boolean(true)])]);
    assert_eq!(bytes, [1, 0, 0, 0, 5, 1, 0, 0, 0, 1, 1]);
}

#[test]
fn a_clone_node_decodes_with_its_nested_draws_and_spans() {
    let bytes = report_bytes(|out| {
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&[5, 0]);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&[1, 1]);
        out.extend_from_slice(&0.5f64.to_bits().to_le_bytes());
        out.push(1);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&9u64.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(0);
        out.push(0);
        out.extend_from_slice(&0u32.to_le_bytes());
        empty_tail(out);
    });
    let report = decode_report(&bytes);
    assert_eq!(
        report.choices(),
        vec![Choice::Clone(vec![Choice::Boolean(true)])]
    );
    let Draw::Clone { nodes, spans } = &report.nodes[0].draw else {
        panic!("expected a clone, got {:?}", report.nodes);
    };
    assert!(nodes[0].forced);
    assert_eq!(spans[0].label, 9);
    assert_eq!(spans[0].parent, None);
}
