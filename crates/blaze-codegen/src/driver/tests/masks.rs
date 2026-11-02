//! Masked IR is made from each build's own IR: what a masked data object is told by (a
//! position's anchor, a file name) is in no key known before preparing, so nothing masked is
//! kept across builds or put in the store; only forms are interned once a process.

use std::sync::atomic::{AtomicU8, AtomicUsize};

use super::*;

/// [`Machine`] whose functions all have one masked form: a body of 100 instructions whose one
/// constant is a data object, told by the function's body and by `data`, which no key holds
/// (as a file name is in none). Its merge counts the members whose group's constants are not
/// the ones their IR gives in this build, as the backend's check refuses them, and then
/// refuses the group, so a build that folds fails. `apart` gives each body its own form, too
/// small for a group to pay.
#[derive(Default)]
struct Folding {
    machine: Machine,
    apart: bool,
    data: AtomicU8,
    stale: AtomicUsize,
    merged: AtomicUsize,
}

impl Folding {
    fn constant(&self, prepared: &Prepared) -> fold::Constant {
        let bytes = [prepared.body, self.data.load(Ordering::Relaxed)];
        fold::Constant::Data(ContentId::of("blaze v2 test folded data 1", &bytes))
    }
}

impl Prepare for Folding {
    type Frontend = Program;
    type Prepared = Prepared;
    type Export = u8;

    fn prepare(
        &self,
        program: &Program,
        unit: &Unit,
        spec: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        self.machine.prepare(program, unit, spec, id, refs)
    }

    fn summarize(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: &mut Prepared,
        refs: &crate::template::Refs,
    ) -> Result<(Summary, u8), CodegenError> {
        self.machine.summarize(unit, spec, prepared, refs)
    }

    fn emit(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: Prepared,
        refs: &crate::template::Refs,
        imports: Option<(&Imports, &[Import<'_, u8>])>,
    ) -> Result<Emitted, CodegenError> {
        self.machine.emit(unit, spec, prepared, refs, imports)
    }

    fn masked(&self, _: &Unit, prepared: &Prepared, _: &crate::template::Refs) -> Option<Masked> {
        let constants = vec![self.constant(prepared)];
        if self.apart {
            return Some(Masked { shape: vec![prepared.body], constants, size: 4 });
        }
        Some(Masked { shape: vec![0xf0], constants, size: 100 })
    }

    fn merge(
        &self,
        group: &Group,
        members: Vec<(&Unit, &Spec, Prepared, &crate::template::Refs)>,
    ) -> Result<fold::Merged, CodegenError> {
        self.merged.fetch_add(1, Ordering::Relaxed);
        for ((_, _, prepared, _), constants) in members.iter().zip(&group.constants) {
            let now = self.constant(prepared);
            if group.varying.iter().zip(constants).any(|(&at, &kept)| at != 0 || kept != now) {
                self.stale.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(error(group.key(), item(0), "folded in a test that compiles no group"))
    }

    fn compile_static(
        &self,
        program: &Program,
        spec: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<Static, CodegenError> {
        self.machine.compile_static(program, spec, id, refs)
    }
}

fn folding_policy() -> Policy {
    let mut policy = policy(OptLevel::O0, Mode::Output);
    policy.fold = true;
    policy
}

/// Two builds of one program with one daemon's masks and store, the second after a change of
/// the data no key holds: the second's group has the constants its members' IR gives now.
#[test]
fn a_group_has_the_data_its_members_ir_gives_in_its_build() {
    let (policy, program) = (folding_policy(), program());
    let (machine, memo, masks) = (Folding::default(), Memo::new(), Masks::new());
    let run = |store: &Store| Build::new(&machine, &program, &policy, &memo).with_masks(&masks).run(&[(1, member())], store);

    let store = Store::default();
    let first = run(&store).expect_err("its group is refused");
    assert!(first.iter().any(|it| it.what.contains("compiles no group")), "{first:?}");
    let merged = machine.merged.load(Ordering::Relaxed);
    assert!(merged > 0, "the functions of one form fold");

    machine.data.store(1, Ordering::Relaxed);
    run(&store).expect_err("its group is refused");
    assert!(machine.merged.load(Ordering::Relaxed) > merged, "the second build folds too");
    assert_eq!(machine.stale.load(Ordering::Relaxed), 0, "a group was made of masked IR from another build");
}

/// Nothing masked goes to the store, and a second build makes each mask again from its IR,
/// interning no new form.
#[test]
fn masked_ir_is_neither_put_nor_kept_and_forms_are_interned_once() {
    let (policy, program) = (folding_policy(), program());
    let machine = Folding { apart: true, ..Folding::default() };
    let (memo, masks) = (Memo::new(), Masks::new());
    let run = |store: &Store| Build::new(&machine, &program, &policy, &memo).with_masks(&masks).run(&[(1, member())], store).unwrap();

    let mut store = Store::default();
    let cold = run(&store);
    assert!(cold.puts.iter().all(|it| it.kind != Kind::Query), "masked IR is put in the store");
    assert!(cold.functions.len() == 5 && cold.stats.folded == 0);
    // 2 and 3 have one body, so one form.
    assert_eq!(masks.forms(), 4);
    store.keep(&cold.puts);
    let forms = masks.forms();
    let warm = run(&store);
    assert_eq!(warm.functions, cold.functions);
    assert!(warm.puts.iter().all(|it| it.kind != Kind::Query), "masked IR is put in the store");
    assert_eq!(masks.forms(), forms, "an unchanged function's form is interned again");
}
