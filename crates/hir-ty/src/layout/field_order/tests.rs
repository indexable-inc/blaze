//! Hot-first layouts of made-up types, laid out by the real calculator as `layout_of_adt` calls
//! it. The fixture tests in `hir-ty-mir` (`tests/layout/field_order.rs`) check the same on
//! programs, through the plan input and the layout query.

use blaze_ids::{ContentId, rng::SplitMix64};
use blaze_mem::Tag;
use hir_def::LocalFieldId;
use la_arena::RawIdx;
use rustc_abi::{
    FieldsShape, Integer, NicheOptimizations, Primitive, ReprOptions, Size, TargetDataLayout,
};
use rustc_index::{Idx as _, IndexSlice};

use super::{FieldLayouts, HotFields, HotFirst, hot_first, same_outside};
use crate::layout::{
    Layout, LayoutCx, LayoutError, RustcEnumVariantIdx, RustcFieldIdx, Variants, scalar_unit,
    tests::sized_fields,
};

fn field(index: u32) -> LocalFieldId {
    LocalFieldId::from_raw(RawIdx::from(index))
}

fn ranks(variants: &[Vec<u32>]) -> HotFields {
    HotFields {
        id: ContentId([0; 32]),
        variants: variants.iter().map(|it| it.iter().map(|&i| field(i)).collect()).collect(),
    }
}

/// The calculator as `layout_of_adt` calls it for a struct (`is_enum` false, sized) or an enum
/// with discriminants `0..`.
fn layout_of(
    cx: &LayoutCx<'_>,
    is_enum: bool,
    repr: &ReprOptions,
    variants: &FieldLayouts<'_, '_>,
) -> Result<Layout, LayoutError> {
    let discriminants = if is_enum { variants.len() } else { 0 };
    Ok(cx.calc.layout_of_struct_or_enum(
        repr,
        variants,
        is_enum,
        NicheOptimizations::Enabled,
        |_, max| (Integer::fit_unsigned(max.last).max(Integer::I8), false),
        (0..discriminants).map(|i| (RustcEnumVariantIdx(i), i as u128)),
        !is_enum,
    )?)
}

fn fields_of<'a, 'b>(variants: &'a [Vec<&'b Layout>]) -> FieldLayouts<'a, 'b> {
    variants.iter().map(|it| it.iter().collect()).collect()
}

/// Variant `v`'s field offsets, memory order and size.
fn variant(
    layout: &Layout,
    v: usize,
) -> (&IndexSlice<RustcFieldIdx, Size>, &[RustcFieldIdx], Size) {
    match &layout.variants {
        Variants::Single { index } if index.0 == v => match &layout.fields {
            FieldsShape::Arbitrary { offsets, in_memory_order } => {
                (offsets.as_slice(), in_memory_order.raw.as_slice(), layout.size)
            }
            other => panic!("a struct laid out as {other:?}"),
        },
        Variants::Multiple { variants, .. } => {
            let it = &variants[RustcEnumVariantIdx(v)];
            (it.field_offsets.as_slice(), &it.fields_in_memory_order().raw, it.size)
        }
        other => panic!("variant {v} of {other:?}"),
    }
}

#[test]
fn a_hot_field_declared_last_is_laid_out_first() {
    let dl = TargetDataLayout::default();
    let cx = LayoutCx::new(&dl);
    let int = |i: Integer| Layout::scalar(&dl, scalar_unit(&dl, Primitive::Int(i, false)));
    let (u64, u8) = (int(Integer::I64), int(Integer::I8));
    // `struct S { a: u64, .., h: u64, last: u8 }`: 65 bytes of fields.
    let mut declared = vec![&u64; 8];
    declared.push(&u8);
    let declared = [declared];
    let fields = fields_of(&declared);
    let with = |repr: &ReprOptions, it: &FieldLayouts<'_, '_>| layout_of(&cx, false, repr, it);
    let rustc = with(&ReprOptions::default(), &fields).unwrap();
    let last = RustcFieldIdx(field(8));
    assert_eq!(
        variant(&rustc, 0).0[last],
        Size::from_bytes(64),
        "rustc puts `last` after the `u64`s"
    );
    blaze_mem::frame::task(Tag::Infer, |frame| {
        let outcome = hot_first(
            &ranks(&[vec![8]]),
            &rustc,
            &fields,
            &ReprOptions::default(),
            &dl,
            frame,
            with,
        );
        let HotFirst::Applied { layout, hot: 1, .. } = outcome else {
            panic!("`last` is not moved first: {outcome:?}")
        };
        let (offsets, in_memory_order, _) = variant(&layout, 0);
        assert_eq!(offsets[last], Size::ZERO);
        assert_eq!(in_memory_order[0], last);
        assert!(same_outside(&rustc, &layout));
    });
}

/// `harness/apps/fieldorder`'s `Row { key: [u32; 4], payload: [u64; 21], weight: u32 }`, whose
/// profile ranks `payload` too (a cold path reads it, within `HOT_RATIO` of the hot loop):
/// all three hot is rustc's order, so the hottest two that share a line are what moves.
#[test]
fn a_big_lukewarm_field_does_not_keep_hot_ones_apart() {
    let dl = TargetDataLayout::default();
    let cx = LayoutCx::new(&dl);
    let int = |i: Integer| Layout::scalar(&dl, scalar_unit(&dl, Primitive::Int(i, false)));
    let (u32, u64) = (int(Integer::I32), int(Integer::I64));
    let array = |of: &Layout, n| cx.calc.layout_of_array_like::<_, _, ()>(of, Some(n)).unwrap();
    let (key, payload) = (array(&u32, 4), array(&u64, 21));
    let declared = [vec![&key, &payload, &u32]];
    let fields = fields_of(&declared);
    let with = |repr: &ReprOptions, it: &FieldLayouts<'_, '_>| layout_of(&cx, false, repr, it);
    let rustc = with(&ReprOptions::default(), &fields).unwrap();
    let (key_at, weight_at) = (RustcFieldIdx(field(0)), RustcFieldIdx(field(2)));
    let offsets = variant(&rustc, 0).0;
    assert_eq!((offsets[key_at].bytes(), offsets[weight_at].bytes()), (0, 184), "rustc's order");
    blaze_mem::frame::task(Tag::Infer, |frame| {
        let outcome = hot_first(
            &ranks(&[vec![0, 2, 1]]),
            &rustc,
            &fields,
            &ReprOptions::default(),
            &dl,
            frame,
            with,
        );
        let HotFirst::Applied { layout, hot: 2, .. } = outcome else {
            panic!("`key` and `weight` are not put together: {outcome:?}")
        };
        let offsets = variant(&layout, 0).0;
        assert_eq!((offsets[key_at].bytes(), offsets[weight_at].bytes()), (0, 16));
        assert_eq!(layout.size, rustc.size);
        assert!(same_outside(&rustc, &layout));
    });
}

#[test]
fn hot_first_never_changes_what_other_types_see() {
    let dl = TargetDataLayout::default();
    let cx = LayoutCx::new(&dl);
    let pool = sized_fields(&dl);
    let mut rng = SplitMix64::new(0x6669_656c_645f_6f72);
    let mut applied = 0;
    for case in 0..600 {
        let is_enum = case % 3 == 0;
        let count = if is_enum { 2 + rng.index(3) } else { 1 };
        let declared: Vec<Vec<&Layout>> = (0..count)
            .map(|_| (0..rng.index(11)).map(|_| &pool[rng.index(pool.len())]).collect())
            .collect();
        let fields = fields_of(&declared);
        let with =
            |repr: &ReprOptions, it: &FieldLayouts<'_, '_>| layout_of(&cx, is_enum, repr, it);
        let Ok(rustc) = with(&ReprOptions::default(), &fields) else { continue };
        let hot: Vec<Vec<u32>> = declared
            .iter()
            .map(|it| {
                let mut picked: Vec<u32> = Vec::new();
                for _ in 0..rng.index(4) {
                    if it.is_empty() {
                        break;
                    }
                    let i = rng.index(it.len()) as u32;
                    if !picked.contains(&i) {
                        picked.push(i);
                    }
                }
                picked
            })
            .collect();
        let outcome = blaze_mem::frame::task(Tag::Infer, |frame| {
            hot_first(&ranks(&hot), &rustc, &fields, &ReprOptions::default(), &dl, frame, with)
        });
        let HotFirst::Applied { layout, .. } = outcome else { continue };
        applied += 1;
        assert!(same_outside(&rustc, &layout), "case {case}: {rustc:?}\nbecame {layout:?}");
        assert_eq!(layout.size, rustc.size, "case {case}");
        assert_eq!(layout.align, rustc.align, "case {case}");
        for (v, own) in declared.iter().enumerate() {
            if matches!(&layout.variants, Variants::Single { index } if index.0 != v) {
                continue;
            }
            let (offsets, in_memory_order, size) = variant(&layout, v);
            assert_eq!(offsets.len(), own.len(), "case {case} variant {v}");
            let mut seen: Vec<usize> = in_memory_order.iter().map(|f| f.index()).collect();
            seen.sort_unstable();
            assert!(
                seen.iter().enumerate().all(|(i, &f)| f == i),
                "case {case}: not a permutation"
            );
            assert!(
                in_memory_order.windows(2).all(|it| offsets[it[0]] <= offsets[it[1]]),
                "case {case} variant {v}: memory order is not by offset"
            );
            let mut end = Size::ZERO;
            for &f in in_memory_order {
                let it = own[f.index()];
                assert_eq!(offsets[f].bytes() % it.align.abi.bytes(), 0, "case {case}: misaligned");
                if it.size > Size::ZERO {
                    assert!(offsets[f] >= end, "case {case} variant {v}: fields overlap");
                    end = offsets[f] + it.size;
                }
            }
            assert!(end <= size, "case {case} variant {v}: a field past the end");
        }
    }
    assert!(applied >= 50, "only {applied} of the cases were reordered");
}

#[test]
fn the_same_ranks_give_the_same_layout() {
    let dl = TargetDataLayout::default();
    let pool = sized_fields(&dl);
    let mut rng = SplitMix64::new(0x7361_6d65);
    for _ in 0..100 {
        let declared: [Vec<&Layout>; 1] =
            [(0..rng.index(12)).map(|_| &pool[rng.index(pool.len())]).collect()];
        let len = declared[0].len() as u32;
        let hot = [(0..len).filter(|_| rng.coin()).collect::<Vec<u32>>()];
        let run = || {
            let cx = LayoutCx::new(&dl);
            let fields = fields_of(&declared);
            let with =
                |repr: &ReprOptions, it: &FieldLayouts<'_, '_>| layout_of(&cx, false, repr, it);
            let rustc = with(&ReprOptions::default(), &fields).ok()?;
            blaze_mem::frame::task(Tag::Infer, |frame| {
                match hot_first(
                    &ranks(&hot),
                    &rustc,
                    &fields,
                    &ReprOptions::default(),
                    &dl,
                    frame,
                    with,
                ) {
                    HotFirst::Applied { layout, hot, tried } => Some((Some(layout), hot, tried)),
                    HotFirst::AlreadyFirst | HotFirst::Shape(_) => Some((None, 0, 0)),
                    HotFirst::NoneFits { tried } => Some((None, 0, tried)),
                }
            })
        };
        assert_eq!(run(), run());
    }
}
