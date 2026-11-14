use blaze_ids::rng::SplitMix64;

use super::*;

/// The byte-at-a-time scan [`hits_of`] must equal.
fn reference(map: &[u8]) -> Vec<u32> {
    map.iter().enumerate().filter(|it| *it.1 != 0).map(|it| it.0 as u32).collect()
}

#[test]
fn the_word_scan_finds_what_the_byte_scan_finds() {
    let mut rng = SplitMix64::new(7);
    for len in 0..200usize {
        for density in [0u64, 1, 8, 64, 255] {
            let map: Vec<u8> = (0..len)
                .map(|_| {
                    let roll = rng.next_u64();
                    match roll % 256 < density {
                        // Any value but zero is a hit, the high bit alone included.
                        true => [1u8, 0x80, 0xff, 0x7f, 2][(roll >> 8) as usize % 5],
                        false => 0,
                    }
                })
                .collect();
            let mut out = vec![9];
            hits_of(&map, &mut out);
            assert_eq!(out[0], 9, "added to what was there");
            assert_eq!(out[1..], reference(&map)[..], "length {len}, density {density}");
        }
    }
}

#[test]
fn the_header_is_the_copy_it_replaced() {
    let kept = |slots: u32| {
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(b"blzexec1");
        out[8..12].copy_from_slice(&slots.to_le_bytes());
        out
    };
    assert_eq!((MAGIC, HEADER), (*b"blzexec1", 16));
    let mut rng = SplitMix64::new(3);
    for slots in
        [0, 1, 255, 256, 65_536, u32::MAX].into_iter().chain((0..64).map(|_| rng.next_u64() as u32))
    {
        assert_eq!(header(slots), kept(slots), "{slots} slots");
    }
}

/// A table of `slots` slots, of one lineage and generation.
fn table(slots: u32) -> Table {
    Table { lineage: [1; 16], generation: 2, slots }
}

#[test]
fn a_region_is_whole_only_when_armed_and_ended() {
    let mut region = header(20).to_vec();
    region.resize(HEADER + 20, 0);
    region[HEADER + 3] = 1;
    region[HEADER + 19] = 1;
    let of = table(20);
    assert!(Record::of_region(&region, of, Ok(())).why().unwrap().contains("armed 0 of its 20"));
    assert!(mark_armed(&mut region));
    assert_eq!(
        Record::of_region(&region, of, Ok(())),
        Record::Complete { table: of, hits: vec![3, 19] }
    );
    assert!(
        Record::of_region(&region, of, Err("ended by signal 9"))
            .why()
            .unwrap()
            .contains("signal 9")
    );
    assert!(Record::of_region(&region[..HEADER + 10], of, Ok(())).why().is_some());
    assert!(Record::of_region(&region[..4], of, Ok(())).why().is_some());
    let mut other = region.clone();
    other[0] = b'x';
    assert!(Record::of_region(&other, of, Ok(())).why().unwrap().contains("magic"));
    assert!(!mark_armed(&mut other));
    assert!(!mark_armed(&mut [0u8; 3]));
}

#[test]
fn hits_out_of_order_or_range_are_no_record() {
    let of = table(4);
    assert_eq!(Record::of_hits(of, vec![0, 3]), Record::Complete { table: of, hits: vec![0, 3] });
    assert_eq!(Record::of_hits(of, Vec::new()), Record::Complete { table: of, hits: Vec::new() });
    assert!(Record::of_hits(of, vec![4]).why().unwrap().contains("slot 4"));
    assert!(Record::of_hits(of, vec![2, 2]).why().unwrap().contains("out of order"));
    assert!(Record::of_hits(of, vec![3, 1]).why().is_some());
}
