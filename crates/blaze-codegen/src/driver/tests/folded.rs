//! Folded groups merged and landed in one daemon's memo across builds: what a build asks the
//! memo for is what the landing it takes holds.

use std::sync::atomic::AtomicUsize;

use super::*;

/// [`Machine`] whose functions of body `0x22` (2 and 3 of [`program`]) have one masked form,
/// a body of 100 instructions calling place 0, which varies between them, so they fold. Its
/// merge makes a body of the group's key alone and a thunk per member calling it, counting
/// each merge.
#[derive(Default)]
struct Merging {
    machine: Machine,
    merged: AtomicUsize,
}

fn compiled(key: InstanceKey, code: Vec<u8>, relocs: Vec<crate::Relocation>) -> Emitted {
    let function = CompiledFunction {
        key,
        arch: Arch::Wasm32,
        align: 16,
        code,
        relocs: relocs.into(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    };
    Emitted { function, side: None, extra: Vec::new() }
}

impl Prepare for Merging {
    type Frontend = Program;
    type Prepared = Prepared;
    type Export = Exported;

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
    ) -> Result<(Summary, Exported), CodegenError> {
        self.machine.summarize(unit, spec, prepared, refs)
    }

    fn emit(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: Prepared,
        refs: &crate::template::Refs,
        imports: Option<(&Imports, &[Import<'_, Exported>])>,
    ) -> Result<Emitted, CodegenError> {
        self.machine.emit(unit, spec, prepared, refs, imports)
    }

    fn masked(&self, _: &Unit, prepared: &Prepared, _: &crate::template::Refs) -> Option<Masked> {
        (prepared.body == 0x22).then(|| Masked { shape: vec![0xf0], constants: vec![fold::Constant::Place(0)], size: 100 })
    }

    fn merge(
        &self,
        group: &Group,
        members: Vec<(&Unit, &Spec, Prepared, &crate::template::Refs)>,
    ) -> Result<fold::Merged, CodegenError> {
        self.merged.fetch_add(1, Ordering::Relaxed);
        let key = group.key();
        let body = compiled(key, vec![0xf0], Vec::new());
        let thunks = (members.iter())
            .map(|(unit, ..)| {
                let call = crate::Relocation { at: 1, kind: RelocKind::PcRel32, target: Target::Direct(key), addend: -4 };
                compiled(unit.instance.key(), vec![0xe9, 0, 0, 0, 0], vec![call])
            })
            .collect();
        Ok(fold::Merged { body, thunks })
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

/// C1 of the fold-key review: a member's thunk is kept under what its code is kept under,
/// which an edit of a body it builds in moves while its instance key, and so the group's key
/// when it is not the first member, stay. Two builds of one memo, as a daemon's: the second,
/// one member building its callee in, takes no landing of the first that lacks the thunk it
/// asks for, but merges again. Each of 2 and 3 is edited in turn, so whichever is not first
/// is.
#[test]
fn a_landed_group_holds_every_thunk_it_is_asked_for() {
    let policy = folding_policy();
    for edited in [2, 3] {
        let mut program = program();
        let (machine, memo) = (Merging::default(), Memo::new());
        let mut store = Store::default();
        let first = Build::new(&machine, &program, &policy, &memo).run(&[(1, member())], &store).unwrap();
        assert_eq!(first.stats.folded, 2, "2 and 3 fold");
        assert_eq!(machine.merged.load(Ordering::Relaxed), 1);
        store.keep(&first.puts);

        program.functions.get_mut(&edited).unwrap().built_in = vec![0];
        let again = Build::new(&machine, &program, &policy, &memo).run(&[(1, member())], &store);
        let again = again.unwrap_or_else(|errors| panic!("f{edited} edited: {errors:?}"));
        assert_eq!(again.stats.folded, 2, "f{edited} edited: 2 and 3 fold");
        assert_eq!(machine.merged.load(Ordering::Relaxed), 2, "f{edited} edited: its thunk is merged again");
    }
}
