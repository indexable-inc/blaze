use blaze_sim::{Config, Tape};

use super::*;

/// The copies these functions replaced, kept as the spec: `blaze-link`'s `reloc::align_up`,
/// `blaze-mem`'s `os::round_up`, `blaze-cas`'s `disk::aligned` and `blaze-stencil`'s
/// `Buf::align` padding, written with division where they could be.
mod reference {
    pub(super) fn up(value: u64, align: u64) -> u64 {
        (value + (align - 1)) & !(align - 1)
    }

    pub(super) fn round_up(bytes: usize, to: usize) -> Option<usize> {
        Some(bytes.checked_add(to - 1)? & !(to - 1))
    }

    pub(super) fn down(value: u64, align: u64) -> u64 {
        value - value % align
    }

    pub(super) fn padding(len: u64, align: u64) -> u64 {
        (align - len % align) % align
    }
}

const _: () = {
    assert!(align_up(0, 8) == 0);
    assert!(align_up(1, 8) == 8);
    assert!(align_up(8, 8) == 8);
    assert!(align_down_u64(0x1fff, 0x1000) == 0x1000);
    assert!(is_aligned_u32(48, 16));
    assert!(!is_aligned_u32(40, 16));
    assert!(padding_for(13, 4) == 3);
    assert!(checked_align_up(usize::MAX, 2).is_none());
    assert!(matches!(checked_align_up_u64(u64::MAX, 1), Some(u64::MAX)));
};

fn power_of_two(tape: &Tape, bits: u32) -> u64 {
    1u64 << tape.int_in(0..=bits - 1, "align shift")
}

#[test]
fn up_matches_the_copies_and_std() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let value: u64 = tape.int("value");
        let align = power_of_two(tape, 64);
        let checked = checked_align_up_u64(value, align);
        assert_eq!(checked, value.checked_next_multiple_of(align));
        assert_eq!(checked.map(|it| it as usize), reference::round_up(value as usize, align as usize));
        if let Some(up) = checked {
            assert_eq!(align_up_u64(value, align), up);
            assert_eq!(reference::up(value, align), up);
            assert_eq!(align_up(value as usize, align as usize), up as usize);
        }
    })
}

#[test]
fn up_u32_matches_std() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let value: u32 = tape.int("value");
        let align = power_of_two(tape, 32);
        let align = align as u32;
        assert_eq!(checked_align_up_u32(value, align), value.checked_next_multiple_of(align));
        if let Some(up) = value.checked_next_multiple_of(align) {
            assert_eq!(align_up_u32(value, align), up);
        }
    })
}

#[test]
fn down_and_is_aligned_match_division() {
    blaze_sim::check(Config::DEFAULT, |tape| down_and_is_aligned(tape))
}

fn down_and_is_aligned(tape: &Tape) {
    let value: u64 = tape.int("value");
    let align = power_of_two(tape, 64);
    let down = align_down_u64(value, align);
    assert_eq!(down, reference::down(value, align));
    assert!(down <= value && value - down < align);
    assert_eq!(is_aligned_u64(value, align), value.is_multiple_of(align));
    assert!(is_aligned_u64(down, align));
    assert_eq!(align_down(value as usize, align as usize), down as usize);
    assert_eq!(
        is_aligned(value as usize, align as usize),
        (value as usize).is_multiple_of(align as usize)
    );
    if align <= 1 << 31 {
        let (small, small_align) = (value as u32, align as u32);
        assert_eq!(align_down_u32(small, small_align), small - small % small_align);
        assert_eq!(is_aligned_u32(small, small_align), small % small_align == 0);
    }
}

#[test]
fn padding_reaches_the_next_multiple() {
    blaze_sim::check(Config::DEFAULT, |tape| padding_reaches(tape))
}

fn padding_reaches(tape: &Tape) {
    let value: u64 = tape.int("value");
    let align = power_of_two(tape, 64);
    let padding = padding_for_u64(value, align);
    assert_eq!(padding, reference::padding(value, align));
    assert!(padding < align);
    if let Some(up) = checked_align_up_u64(value, align) {
        assert_eq!(value + padding, up);
    }
    assert_eq!(padding_for(value as usize, align as usize), padding as usize);
    if align <= 1 << 31 {
        let (small, small_align) = (value as u32, align as u32);
        assert_eq!(
            padding_for_u32(small, small_align),
            (small_align - small % small_align) % small_align
        );
    }
}

/// Against `blaze-live`'s `pages`, which `align_range_u64` replaced.
#[test]
fn range_covers_the_blocks_touched() {
    blaze_sim::check(Config::DEFAULT, |tape| range_covers(tape))
}

fn range_covers(tape: &Tape) {
    let start: u64 = tape.int_in(0..=(u64::MAX >> 1) - 1, "start");
    let len: u64 = tape.int_in(0..=(1 << 20) - 1, "len");
    let align = power_of_two(tape, 32);
    let range = start..start + len;
    let covered = align_range_u64(range.clone(), align);
    assert_eq!(covered, range.start & !(align - 1)..range.end.next_multiple_of(align));
    assert!(covered.start <= range.start && covered.end >= range.end);
    assert!(is_aligned_u64(covered.start, align) && is_aligned_u64(covered.end, align));
    assert_eq!(
        align_range(start as usize..(start + len) as usize, align as usize),
        covered.start as usize..covered.end as usize
    );
}

#[test]
fn edges() {
    for shift in 0..64 {
        let align = 1u64 << shift;
        assert_eq!(align_up_u64(0, align), 0);
        assert_eq!(align_up_u64(align, align), align);
        assert_eq!(align_down_u64(u64::MAX, align), u64::MAX - (align - 1));
        assert_eq!(padding_for_u64(u64::MAX, align), 1 % align);
        let top = u64::MAX - (align - 1);
        assert_eq!(checked_align_up_u64(top, align), Some(top));
        if align > 1 {
            assert_eq!(checked_align_up_u64(top + 1, align), None);
        }
    }
}
