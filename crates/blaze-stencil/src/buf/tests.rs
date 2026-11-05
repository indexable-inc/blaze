//! Islands: synthetic AArch64 functions whose branches and data references cross each form's
//! reach by one step, and land where they mean to after [`Buf::resolve`].

use blaze_mem::{Tag, frame};

use super::*;

const NOP: u32 = 0xd503_201f;
const B: u32 = 0x1400_0000;
const B_EQ: u32 = 0x5400_0000;
const TBZ: u32 = 0x3600_0000;
const ADR: u32 = 0x1000_0000;
const LDR: u32 = 0x5800_0000;

/// The library's `jump` stencil on AArch64: one `b`, its field the whole word.
fn island() -> Island<'static> {
    const JUMP: [u8; 4] = B.to_le_bytes();
    const PAD: [u8; 4] = NOP.to_le_bytes();
    Island { jump: &JUMP, at: 0, form: Form::A64Jump26, addend: 0, nop: &PAD, align: 16 }
}

/// A function written the way the patcher writes one: room before each 4-byte piece.
struct Prog<'f> {
    buf: Buf<'f>,
    island: Option<Island<'static>>,
    /// By data label, its datum.
    data: Vec<(Label, [u8; 16])>,
}

impl<'f> Prog<'f> {
    fn new(frame: &'f Frame, bound: usize, island: Option<Island<'static>>) -> Prog<'f> {
        let mut buf = Buf::new_in(frame);
        buf.reserve(bound, 16, 16);
        Prog { buf, island, data: Vec::new() } // alloc: test
    }

    fn room(&mut self, bytes: u32) -> Result<(), Error> {
        let data = &self.data;
        self.buf.room(bytes, self.island.as_ref(), |label| {
            data.iter().find(|it| it.0 == label).map(|it| &it.1[..])
        })
    }

    fn nops(&mut self, count: u32) -> Result<(), Error> {
        for _ in 0..count {
            self.room(4)?;
            self.buf.put4(NOP);
        }
        Ok(())
    }

    /// Nops up to offset `to`, when no island moved the code past it.
    fn nops_to(&mut self, to: u32) -> Result<(), Error> {
        while self.buf.len() < to {
            self.nops(1)?;
        }
        Ok(())
    }

    /// An instruction with a field of `form` to `label`; its offset.
    fn refer(&mut self, word: u32, form: Form, label: Label) -> Result<u32, Error> {
        self.room(4)?;
        let at = self.buf.len();
        self.buf.put4(word);
        self.buf.fixup(at, form, label, 0);
        Ok(at)
    }

    fn datum(&mut self, seed: u8) -> Label {
        let label = self.buf.label();
        self.data.push((label, core::array::from_fn(|i| seed.wrapping_add(i as u8))));
        label
    }

    /// The constant area, the trailing island and the fixups, as the patcher ends a function.
    fn end(mut self) -> Result<Vec<u8>, Error> {
        for at in 0..self.data.len() {
            let (label, bytes) = self.data[at];
            if self.buf.offset(label).is_none() {
                self.room(32)?;
                self.buf.align(16, &NOP.to_le_bytes());
                self.buf.bind(label);
                self.buf.put(&bytes);
            }
        }
        let data = &self.data;
        self.buf.settle(self.island.as_ref(), |label| {
            data.iter().find(|it| it.0 == label).map(|it| &it.1[..])
        })?;
        self.buf.resolve()?;
        Ok(self.buf.bytes().to_vec()) // alloc: test
    }
}

fn word(code: &[u8], at: u32) -> u32 {
    u32::from_le_bytes(code[at as usize..at as usize + 4].try_into().unwrap())
}

/// Where the field of `form` at `at` goes: straight there, or through one veneer (a `b`).
fn follow(code: &[u8], at: u32, form: Form) -> (u32, bool) {
    let to = (i64::from(at) + enc::read(form, code, at as usize)) as u32;
    if form.reach().unwrap().far == Far::Veneer && word(code, to) & 0xfc00_0000 == B {
        ((i64::from(to) + enc::read(Form::A64Jump26, code, to as usize)) as u32, true)
    } else {
        (to, false)
    }
}

fn with<R>(f: impl FnOnce(&Frame) -> R) -> R {
    frame::task(Tag::Codegen, f)
}

/// A function of `bound` bytes whose only branch, of `form`, goes `distance` bytes back or
/// ahead, then `tail` bytes of code: the branch lands on its label, through a veneer exactly
/// when the label is past its reach.
fn branch(form: Form, insn: u32, distance: i64, tail: u32) -> (Vec<u8>, bool) {
    with(|frame| {
        let bound = distance.unsigned_abs() as usize + tail as usize + 64;
        let mut prog = Prog::new(frame, bound, Some(island()));
        let label = prog.buf.label();
        prog.nops(2).unwrap();
        let (at, target) = if distance < 0 {
            prog.buf.bind(label);
            let target = prog.buf.len();
            prog.nops_to(target + distance.unsigned_abs() as u32).unwrap();
            (prog.refer(insn, form, label).unwrap(), target)
        } else {
            let at = prog.refer(insn, form, label).unwrap();
            prog.nops_to(at + distance as u32).unwrap();
            prog.buf.bind(label);
            let target = prog.buf.len();
            (at, target)
        };
        prog.nops(tail / 4).unwrap();
        let code = prog.end().unwrap_or_else(|e| panic!("{form:?} {distance}: {e}"));
        let (to, veneer) = follow(&code, at, form);
        assert_eq!(to, target, "{form:?} over {distance} bytes lands off its label");
        (code, veneer)
    })
}

#[test]
fn a_branch_one_step_past_its_reach_goes_through_one_veneer() {
    for (form, insn) in [(Form::A64CondBr19, B_EQ), (Form::A64TestBr14, TBZ)] {
        let reach = form.reach().unwrap();
        for tail in [8, 3 << 20] {
            // Back: exactly at its reach it is written as is, and no island is needed.
            let (code, veneer) = branch(form, insn, reach.back, tail);
            assert!(
                !veneer,
                "{form:?} back {} with {tail} after went through a veneer",
                reach.back
            );
            assert_eq!(
                code.len() as i64,
                8 - reach.back + 4 + i64::from(tail),
                "{form:?}: an island no branch needed"
            );
            let (_, veneer) = branch(form, insn, reach.back - 4, tail);
            assert!(veneer, "{form:?} back {} reached without a veneer", reach.back - 4);
            // Ahead: the label is unbound when the branch is written, so its island comes
            // before the label could pass the reach; past the reach, only a veneer gets there.
            let (_, veneer) = branch(form, insn, reach.ahead + 4, tail);
            assert!(veneer, "{form:?} ahead {} reached without a veneer", reach.ahead + 4);
            branch(form, insn, reach.ahead, tail);
        }
        // Short branches in a function the form reaches across need nothing.
        let (code, veneer) = branch(form, insn, 64, 64);
        assert!(!veneer);
        assert_eq!(code.len(), 8 + 64 + 64);
    }
}

#[test]
fn a_datum_past_the_reach_of_its_use_is_placed_within_it() {
    for (form, insn) in [(Form::A64AdrLo21, ADR), (Form::A64LdrLit19, LDR)] {
        with(|frame| {
            let size = 3u32 << 20;
            let mut prog = Prog::new(frame, size as usize + 4096, Some(island()));
            let mut uses = Vec::new(); // alloc: test
            for seed in 0..4u8 {
                let label = prog.datum(seed * 16);
                let at = prog.refer(insn, form, label).unwrap();
                uses.push((at, seed));
                prog.nops_to(u32::from(seed + 1) * (size / 5)).unwrap();
            }
            let code = prog.end().unwrap_or_else(|e| panic!("{form:?}: {e}"));
            for (at, seed) in uses {
                let (to, _) = follow(&code, at, form);
                assert_eq!(
                    &code[to as usize..to as usize + 16],
                    &core::array::from_fn::<u8, 16, _>(|i| seed * 16 + i as u8)
                );
                assert_eq!(to % 16, 0, "{form:?}: datum {seed} placed off its alignment");
            }
        });
    }
}

/// A use behind a datum placed out of its reach is told to take a new label.
#[test]
fn a_datum_placed_behind_out_of_reach_is_placed_again() {
    with(|frame| {
        let mut prog = Prog::new(frame, 2 << 20, Some(island()));
        let label = prog.datum(7);
        prog.buf.bind(label);
        prog.buf.put(&[7; 16]);
        let reach = Form::A64AdrLo21.reach().unwrap();
        let at = (-reach.back) as u32;
        assert!(!prog.buf.placed_out_of_reach(at, Form::A64AdrLo21, label, 0));
        assert!(prog.buf.placed_out_of_reach(at + 1, Form::A64AdrLo21, label, 0));
    });
}

/// Many crossing branches of every kind: each lands on its label, and the code is the same
/// bytes each time it is written.
#[test]
fn crossing_branches_land_and_write_the_same_bytes() {
    let write = || {
        with(|frame| {
            let size = 5u32 << 20;
            let mut prog = Prog::new(frame, size as usize + 4096, Some(island()));
            let labels: Vec<Label> = (0..48).map(|_| prog.buf.label()).collect(); // alloc: test
            let mut uses = Vec::new(); // alloc: test
            let mut targets = vec![0; labels.len()]; // alloc: test
            let mut rng = blaze_ids::rng::SplitMix64::new(0x6272_616e);
            for (n, &label) in labels.iter().enumerate() {
                prog.nops_to(size / labels.len() as u32 * n as u32).unwrap();
                for _ in 0..3 {
                    let other = labels[rng.next_u64() as usize % labels.len()];
                    let (form, insn) =
                        [(Form::A64CondBr19, B_EQ), (Form::A64TestBr14, TBZ), (Form::A64Jump26, B)]
                            [rng.next_u64() as usize % 3];
                    uses.push((prog.refer(insn, form, other).unwrap(), form, other.0 as usize));
                }
                prog.buf.bind(label);
                targets[n] = prog.buf.len();
                prog.nops(1).unwrap();
            }
            let code = prog.end().unwrap_or_else(|e| panic!("{e}"));
            for (at, form, label) in uses {
                assert_eq!(
                    follow(&code, at, form).0,
                    targets[label],
                    "{form:?} at {at:#x} to label {label}"
                );
            }
            code
        })
    };
    assert_eq!(write(), write());
}

/// Without a `jump` stencil there is no island: a field about to pass its reach is an error
/// naming it, never other code.
#[test]
fn a_field_out_of_reach_with_no_island_is_an_error_naming_it() {
    with(|frame| {
        let mut prog = Prog::new(frame, 2 << 20, None);
        let label = prog.buf.label();
        prog.refer(B_EQ, Form::A64CondBr19, label).unwrap();
        let error = prog.nops(1 << 18).unwrap_err().to_string();
        assert!(error.contains("A64CondBr19 field at 0x0"), "{error}");
    });
}

/// Reaches no function gets past are never near: x86-64's `rel32`, and every AArch64 form in
/// a function short enough.
#[test]
fn a_short_function_tracks_nothing() {
    with(|frame| {
        let mut buf = Buf::new_in(frame);
        buf.reserve(60 << 10, 4, 4);
        let label = buf.label();
        for (form, word) in [(Form::A64CondBr19, B_EQ), (Form::A64AdrLo21, ADR), (Form::Rel32, 0)] {
            let at = buf.len();
            buf.put4(word);
            buf.fixup(at, form, label, 0);
        }
        assert_eq!(buf.due, NEVER);
        let mut buf = Buf::new_in(frame);
        buf.reserve(1 << 30, 4, 4);
        let label = buf.label();
        buf.put4(0);
        buf.fixup(0, Form::Rel32, label, -4);
        assert_eq!(buf.due, NEVER);
    });
}
