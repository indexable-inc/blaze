//! The build over a front end and backend made up here: what is compiled, kept, shared and
//! renamed, without a database.

use std::{
    collections::{BTreeMap, HashMap},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use blaze_core::lock::Lock;

use super::*;
use crate::{
    Arch, Backend, Init, RelocKind,
    plan::{Mode, OptLevel, Override, Package, Profile},
    settings::{DebugInfo, Panic, Pgo, Relocation, Sanitize, Toolchain},
    summary::{Attrs, Call, Hint},
};

#[derive(Clone)]
struct Function {
    body: u8,
    calls: Vec<u32>,
    statics: Vec<u32>,
    origin: Origin,
    symbol: Option<String>,
    /// How `symbol` is defined ([`Described::linkage`]).
    linkage: crate::Linkage,
    /// The places among its references it builds in.
    built_in: Vec<u32>,
    /// The outside symbol it calls, its last reference: no part of its code hash, so two
    /// builds can differ in it with every key the same.
    outside: &'static str,
    /// The function whose code it is, itself when `None`: two of one code have one key, as
    /// two instances of a generic whose code reads their arguments alike do.
    code_of: Option<u32>,
    /// Its owner for the dependency image, which the front end decides
    /// ([`Described::owner`]): the workspace's unless a test says.
    owner: Owner,
    /// The function whose identity it has ([`Described::identity`]), itself when `None`: two
    /// of one identity are one instance asked for by two origins.
    identity_of: Option<u32>,
    /// Its own span tables' anchors, when its template is kept under a template code with no
    /// name and no place in it ([`Described::template_code`]): its body and callees alone.
    /// Its compiled code then names a position in two of them.
    anchors: Option<[ContentId; crate::template::OWN_TABLES]>,
}

struct Program {
    functions: HashMap<u32, Function>,
    /// Each static's value byte and section.
    statics: HashMap<u32, (u8, Option<String>)>,
    /// The functions a static's value points to.
    static_calls: HashMap<u32, Vec<u32>>,
    /// The functions of each level the walk reached, in the order it told them.
    reached: Lock<Vec<Vec<u32>>>,
    stop: AtomicBool,
    /// Each counted function's body and whether it is drop glue: none is counted unless named.
    bodies: HashMap<u32, (u32, bool)>,
    /// The recursion limit of crate 0, every function's unless [`Self::crates`] says: `None`,
    /// none is held to one.
    recursion_limit: Option<u32>,
    /// Each function's crate, when not 0, and whether it is collected in its own crate's build
    /// (not generic, not `#[inline]`).
    crates: HashMap<u32, (u32, bool)>,
    /// The recursion limit of each crate other than 0.
    limits: HashMap<u32, u32>,
}

impl Default for Program {
    fn default() -> Program {
        Program {
            functions: HashMap::default(),
            statics: HashMap::default(),
            static_calls: HashMap::default(),
            reached: Lock::new("test.codegen.driver.reached", Vec::new()),
            stop: AtomicBool::new(false),
            bodies: HashMap::default(),
            recursion_limit: None,
            crates: HashMap::default(),
            limits: HashMap::default(),
        }
    }
}

impl Program {
    /// The code hash of `id`. The code takes callees by their interface, here their number:
    /// not their bodies.
    fn code(&self, id: u32) -> CodeHash {
        let function = &self.functions[&id];
        let mut code = Hasher::new("blaze v2 test code 1");
        code.update(&function.code_of.unwrap_or(id).to_le_bytes()).update(&[function.body]);
        if function.code_of.is_none() {
            for callee in &function.calls {
                code.update(&callee.to_le_bytes());
            }
        }
        CodeHash(code.finish())
    }

    /// The code hashes of the bodies `id` builds in, through what they build in, as the
    /// driver closes them: its MIR inliner's copies.
    fn built_in(&self, id: u32) -> Vec<CodeHash> {
        let (mut seen, mut queue) = (Vec::new(), vec![id]);
        while let Some(at) = queue.pop() {
            let function = &self.functions[&at];
            for place in &function.built_in {
                let Some(&callee) = function.calls.get(*place as usize) else { continue };
                if callee != id && !seen.contains(&callee) {
                    seen.push(callee);
                    queue.push(callee);
                }
            }
        }
        seen.into_iter().map(|it| self.code(it)).collect()
    }

    fn function(&mut self, id: u32, body: u8, calls: &[u32]) -> &mut Function {
        let function = Function {
            body,
            calls: calls.to_vec(),
            statics: Vec::new(),
            origin: Origin::Member(Package::new("app")),
            symbol: None,
            linkage: crate::Linkage::Strong,
            built_in: Vec::new(),
            outside: "memcpy",
            code_of: None,
            owner: Owner::Workspace,
            identity_of: None,
            anchors: None,
        };
        self.functions.insert(id, function);
        self.functions.get_mut(&id).unwrap()
    }
}

fn item(id: u32) -> ItemKey {
    ItemKey(ContentId::of("blaze v2 test item 1", &id.to_le_bytes()))
}

fn static_item(id: u32) -> ItemKey {
    ItemKey(ContentId::of("blaze v2 test static 1", &id.to_le_bytes()))
}

impl Frontend for Program {
    type Instance = u32;
    type Static = u32;

    fn describe(&self, id: &u32, _: &Origin) -> Result<Described<u32, u32>, CodegenError> {
        let function = &self.functions[id];
        let refs = function
            .calls
            .iter()
            .map(|it| FrontRef::Function(*it))
            .chain(function.statics.iter().map(|it| FrontRef::Static(*it)))
            .chain([FrontRef::Extern(function.outside.to_owned())])
            .collect();
        let code = self.code(*id);
        let template_code = match function.anchors {
            Some(_) => {
                let mut template = Hasher::new("blaze v2 test template code 1");
                template.update(&[function.body]);
                for callee in &function.calls {
                    template.update(&callee.to_le_bytes());
                }
                CodeHash(template.finish())
            }
            None => code,
        };
        Ok(Described {
            owner: function.owner,
            identity: ContentId::of(
                "blaze v2 test identity 1",
                &function.identity_of.unwrap_or(*id).to_le_bytes(),
            ),
            bound: Ok(code.0),
            item: item(*id),
            code,
            template_code,
            anchors: function.anchors,
            built_in_code: code,
            args: ContentId::default(),
            origin: function.origin,
            editable: matches!(function.origin, Origin::Member(_)),
            overflow_checks: None,
            address: match function.symbol {
                Some(_) => Address::Significant,
                None => Address::Free,
            },
            symbol: function.symbol.clone(),
            linkage: function.linkage,
            refs,
            built_in: function.built_in.clone(),
            name: format!("f{id}"),
            cold: false,
        })
    }

    fn describe_static(&self, id: &u32) -> Result<DescribedStatic<u32, u32>, CodegenError> {
        let (value, section) = &self.statics[id];
        Ok(DescribedStatic {
            item: static_item(*id),
            instance: ContentId::default(),
            value: ContentId::of("blaze v2 test value 1", &[*value]),
            mutable: false,
            thread_local: false,
            section: section.clone(),
            origin: Origin::Member(Package::new("app")),
            owner: Owner::Workspace,
            symbol: None,
            linkage: crate::Linkage::Strong,
            name: format!("app::S{id}"),
            refs: self
                .static_calls
                .get(id)
                .into_iter()
                .flatten()
                .map(|it| FrontRef::Function(*it))
                .collect(),
        })
    }

    fn reached(&self, functions: &[(u32, Origin)], _: &[u32]) {
        self.reached.lock().push(functions.iter().map(|it| it.0).collect());
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn counted(&self, functions: &[(u32, Origin)]) -> Vec<crate::backend::Counted> {
        (functions.iter())
            .map(|(id, _)| {
                let body =
                    self.bodies.get(id).map(|&(body, drop_glue)| crate::backend::Instantiates {
                        body: ContentId::of("blaze v2 test body 1", &body.to_le_bytes()),
                        drop_glue,
                    });
                let (krate, heads_own_path) = self.crates.get(id).copied().unwrap_or((0, false));
                let value = if krate == 0 {
                    self.recursion_limit
                } else {
                    self.limits.get(&krate).copied()
                };
                let limit = value.map(|value| crate::backend::Limit {
                    krate: ContentId::of("blaze v2 test crate 1", &krate.to_le_bytes()),
                    value,
                });
                crate::backend::Counted { body, limit, heads_own_path }
            })
            .collect()
    }

    fn spell(&self, id: &u32) -> String {
        format!("f{id}")
    }

    fn past_recursion_limit(
        &self,
        id: &u32,
        depth: u32,
        limit: u32,
        head: &u32,
        limit_of: &u32,
    ) -> CodegenError {
        let what = format!(
            "f{id} repeats its body {depth} times on its path from f{head}, past {limit} of f{limit_of}'s crate"
        );
        error(InstanceKey(ContentId::default()), item(*id), what)
    }
}

struct Machine {
    prepared: AtomicUsize,
    emitted: AtomicUsize,
    summarized: AtomicUsize,
    statics: AtomicUsize,
    /// The body byte of each function emitted, in the order emitted.
    emitted_bodies: Lock<Vec<u8>>,
    /// Added to each prepared function's shape when not 0: IR that differs from another
    /// machine's under the same key, as a key missing an input would let through.
    salt: u8,
    /// A code the release compile of each function of this body byte says it built in, which
    /// nothing of the build is: as a backend that built in a body its key misses would.
    unkept: Option<(u8, CodeHash)>,
}

impl Default for Machine {
    fn default() -> Machine {
        Machine {
            prepared: AtomicUsize::new(0),
            emitted: AtomicUsize::new(0),
            summarized: AtomicUsize::new(0),
            statics: AtomicUsize::new(0),
            emitted_bodies: Lock::new("test.codegen.driver.emitted_bodies", Vec::new()),
            salt: 0,
            unkept: None,
        }
    }
}

/// Whether a function of body byte `body` is one nothing may import ([`Hint::Never`]): the
/// bodies `0x6_`.
fn never_imported(body: u8) -> bool {
    body & 0xf0 == 0x60
}

/// The body byte of a function whose IR can't be built, as a layout error stops it.
const FAILS_ITS_CHECK: u8 = 0xee;

/// A function's body byte, and how many places it has.
struct Prepared {
    body: u8,
    refs: usize,
    /// The code hashes of the bodies it builds in ([`Program::built_in`]).
    built_in: Vec<CodeHash>,
    /// [`Function::anchors`]: its code names a position in its own tables.
    own: Option<[ContentId; crate::template::OWN_TABLES]>,
}

/// A release function as its importers build it in: its body byte, which shows in their
/// code, and the code it is and builds in, which their [`CompiledFunction::inlined`] names.
struct Exported {
    body: u8,
    code: CodeHash,
    built_in: Vec<CodeHash>,
}

fn bcmp() -> Target {
    Target::Extern("bcmp".to_owned())
}

fn code(prepared: &Prepared) -> Vec<u8> {
    let mut code = vec![prepared.body];
    code.extend(std::iter::repeat_n([0xe8, 0, 0, 0, 0], prepared.refs).flatten());
    code
}

impl Prepare for Machine {
    type Frontend = Program;
    type Prepared = Prepared;
    type Export = Exported;

    fn prepare(
        &self,
        program: &Program,
        unit: &Unit,
        _: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        self.prepared.fetch_add(1, Ordering::Relaxed);
        let function = &program.functions[id];
        let prepared = Prepared {
            body: function.body,
            refs: refs.len(),
            built_in: program.built_in(*id),
            own: function.anchors,
        };
        if prepared.body == FAILS_ITS_CHECK {
            return Err(error(unit.instance.key(), unit.item, "a layout too big for the target"));
        }
        let mut shaped = code(&prepared);
        if self.salt != 0 {
            shaped.push(self.salt);
        }
        Ok((Shape::of(unit, &shaped), prepared))
    }

    fn summarize(
        &self,
        unit: &Unit,
        _: &Spec,
        prepared: &mut Prepared,
        refs: &crate::template::Refs,
    ) -> Result<(Summary, Exported), CodegenError> {
        self.summarized.fetch_add(1, Ordering::Relaxed);
        let calls = refs
            .iter()
            .filter_map(|it| match it {
                Target::Function(key) | Target::Direct(key) => {
                    Some(Call { callee: *key, count: 1, args: Vec::new() })
                }
                _ => None,
            })
            .collect();
        let summary = Summary {
            key: unit.instance.key(),
            size: 4,
            hint: if never_imported(prepared.body) { Hint::Never } else { Hint::None },
            calls,
            addressed: Vec::new(),
            params: 0,
            unused: Vec::new(),
            attrs: Attrs::default(),
            body: Attrs::default(),
            external: true,
        };
        let exported = Exported {
            body: prepared.body,
            code: unit.instance.code,
            built_in: prepared.built_in.clone(),
        };
        Ok((summary, exported))
    }

    fn emit(
        &self,
        unit: &Unit,
        _: &Spec,
        prepared: Prepared,
        refs: &crate::template::Refs,
        imports: Option<(&Imports, &[Import<'_, Exported>])>,
    ) -> Result<Emitted, CodegenError> {
        self.emitted.fetch_add(1, Ordering::Relaxed);
        self.emitted_bodies.lock().push(prepared.body);
        let mut relocs: Vec<crate::Relocation> = (0..prepared.refs)
            .map(|at| crate::Relocation {
                at: 2 + 5 * at as u32,
                kind: RelocKind::PcRel32,
                target: refs[at].clone(),
                addend: -4,
            })
            .collect();
        // A release compile calls `bcmp` of its own accord, as LLVM does for a `memcmp`.
        let mut extra = Vec::new();
        if imports.is_some() {
            relocs.push(crate::Relocation {
                at: 0,
                kind: RelocKind::PcRel32,
                target: bcmp(),
                addend: -4,
            });
            extra.push("bcmp".to_owned());
        }
        let mut code = code(&prepared);
        // What it built in shows in its code, as an import's body would, and its code hash is
        // named, with what the import builds in, as LLVM's release backend names them.
        let mut inlined = Vec::new();
        for import in imports.map_or(&[][..], |it| it.1) {
            code.push(import.export.body);
            inlined.push(import.export.code);
            inlined.extend_from_slice(&import.export.built_in);
        }
        if let Some((body, unkept)) = self.unkept
            && imports.is_some()
            && body == prepared.body
        {
            inlined.push(unkept);
        }
        inlined.sort_unstable();
        inlined.dedup();
        // A panic's location in its own tables, as a `Location` it passes, and its line row.
        let mut lines = Vec::new();
        if let Some(own) = prepared.own {
            let at = code.len() as u32 + 1;
            code.extend([0xe8, 0, 0, 0, 0]);
            let span = blaze_format::SpanRef { anchor: own[1], index: 7 };
            relocs.push(crate::Relocation {
                at,
                kind: RelocKind::PcRel32,
                target: Target::Position(span),
                addend: -4,
            });
            lines.push(crate::Line {
                at: 0,
                span: blaze_format::SpanRef { anchor: own[0], index: 2 },
            });
        }
        let function = CompiledFunction {
            key: unit.instance.key(),
            arch: unit.arch,
            align: 16,
            code,
            relocs: relocs.into(),
            data: Vec::new(),
            inlined,
            side: None,
            unwind: None,
            lines,
            coverage: None,
            profile: None,
        };
        Ok(Emitted { function, side: None, extra })
    }

    fn masked(&self, _: &Unit, _: &Prepared, _: &crate::template::Refs) -> Option<Masked> {
        None
    }

    fn merge(
        &self,
        group: &Group,
        _: Vec<(&Unit, &Spec, Prepared, &crate::template::Refs)>,
    ) -> Result<fold::Merged, CodegenError> {
        Err(error(group.key(), item(0), "nothing folds here"))
    }

    fn compile_static(
        &self,
        program: &Program,
        _: &Spec,
        id: &u32,
        _: &crate::template::Refs,
    ) -> Result<Static, CodegenError> {
        self.statics.fetch_add(1, Ordering::Relaxed);
        let (value, section) = &program.statics[id];
        Ok(Static {
            item: static_item(*id),
            instance: ContentId::default(),
            align: 8,
            init: Init::Bytes { bytes: vec![*value; 8], relocs: crate::Relocs::new() },
            data: Vec::new(),
            mutable: false,
            thread_local: false,
            section: section.clone(),
            imports: Vec::new(),
        })
    }
}

#[derive(Default)]
struct Store(BTreeMap<Key, Vec<u8>>);

impl Lookup for Store {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        Ok(self.0.get(key).map(Vec::as_slice))
    }
}

impl Store {
    fn keep(&mut self, puts: &[Put]) {
        for put in puts {
            self.0.insert(put.key, put.bytes.clone());
        }
    }
}

/// A policy whose code folds (#1292): wasm32's, the one target whose code is Cranelift's.
fn folding_policy() -> Policy {
    let mut policy = policy(OptLevel::O0, Mode::Output);
    policy.toolchain.target = "wasm32-wasip1".to_owned();
    policy.toolchain.arch = Arch::Wasm32;
    policy.toolchain.stencils = None;
    policy.fold = true;
    policy
}

fn policy(level: OptLevel, mode: Mode) -> Policy {
    Policy {
        toolchain: Toolchain {
            id: ContentId([1; 32]),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            arch: Arch::X86_64,
            cpu: "x86-64-v3".to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: Some("23.1".to_owned()),
            stencils: Some("ab".repeat(32)),
            frame_pointers: false,
            relocation: Relocation::Pic,
        },
        profile: Profile {
            opt_level: level,
            overflow_checks: true,
            debug_assertions: true,
            debuginfo: DebugInfo::Full,
            panic: Panic::Unwind,
            packages: BTreeMap::new(),
            build_override: Override::default(),
            stencils: false,
        },
        mode,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
        proc_macro_bridge: false,
        report: false,
        fuel: false,
        heap_to_stack: false,
    }
}

fn member() -> Origin {
    Origin::Member(Package::new("app"))
}

/// `main` (1) calls 2 and 3; 2 calls 4, 3 calls 5; 4 and 5 differ; 2 and 3 are one shape.
fn program() -> Program {
    let mut program = Program::default();
    program.function(1, 0x11, &[2, 3]).symbol = Some("main".to_owned());
    program.function(2, 0x22, &[4]);
    program.function(3, 0x22, &[5]);
    program.function(4, 0x44, &[]);
    program.function(5, 0x55, &[]);
    program
}

fn build(
    program: &Program,
    machine: &Machine,
    policy: &Policy,
    memo: &Memo,
    store: &Store,
) -> Result<Built, Vec<CodegenError>> {
    Build::new(machine, program, policy, memo).run(&[(1, member())], store)
}

#[test]
fn a_cold_build_compiles_once_per_shape_and_a_warm_one_compiles_nothing() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let (machine, memo, mut store) = (Machine::default(), Memo::new(), Store::default());
    let cold = build(&program, &machine, &policy, &memo, &store).unwrap();
    assert_eq!(cold.stats.instances, 5);
    // 2 and 3 are one shape up to their callees: one compile, one memo hit.
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 4);
    assert_eq!(cold.stats.memo, 1);
    assert_eq!(cold.functions.len(), 5);
    assert_eq!(cold.roots.len(), 1);
    assert_eq!(cold.symbols, vec![(cold.roots[0], "main".to_owned(), crate::Linkage::Strong)]);

    store.keep(&cold.puts);
    let warm_machine = Machine::default();
    let warm = build(&program, &warm_machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm_machine.prepared.load(Ordering::Relaxed), 0);
    assert_eq!(warm_machine.emitted.load(Ordering::Relaxed), 0);
    assert_eq!(warm.stats.stored, 5);
    assert_eq!(warm.functions, cold.functions);
}

/// Two items written alike, whose template code holds no name and no place
/// (docs/v2/plan/design/name-free-identity.md): one compile and one template in the store,
/// and each function made of it at its own positions, a warm build taking both from that one
/// entry. Kept under their codes, the same two are two templates.
#[test]
fn two_items_written_alike_share_one_template_each_at_its_own_positions() {
    let anchors = |n: u8| -> [ContentId; crate::template::OWN_TABLES] {
        std::array::from_fn(|t| ContentId([n + t as u8; 32]))
    };
    let (two, three) = (anchors(0x20), anchors(0x30));
    let program_of = |relative: bool| {
        let mut program = program();
        program.function(3, 0x22, &[4]);
        if relative {
            program.functions.get_mut(&2).unwrap().anchors = Some(two);
            program.functions.get_mut(&3).unwrap().anchors = Some(three);
        }
        program
    };
    let templates = |built: &Built| {
        let keys: std::collections::BTreeSet<Key> =
            built.puts.iter().filter(|it| it.kind == Kind::Code).map(|it| it.key).collect();
        keys.len()
    };
    let at_own = |built: &Built, own: &[ContentId; crate::template::OWN_TABLES]| {
        let function = (built.functions.iter())
            .find(|it| it.lines.iter().any(|line| line.span.anchor == own[0]))
            .expect("a function at its own positions");
        let located = blaze_format::SpanRef { anchor: own[1], index: 7 };
        assert!(
            function.relocs.targets().contains(&Target::Position(located)),
            "its panic location is its own"
        );
        function.key
    };
    let policy = policy(OptLevel::O0, Mode::Output);
    let (program, machine, memo, mut store) =
        (program_of(true), Machine::default(), Memo::new(), Store::default());
    let cold = build(&program, &machine, &policy, &memo, &store).unwrap();
    assert_ne!(at_own(&cold, &two), at_own(&cold, &three), "two functions, each its own instance");
    assert_eq!(
        machine.emitted.load(Ordering::Relaxed),
        3,
        "`main`, `4` and one compile of the two"
    );

    let owned_program = program_of(false);
    let owned =
        build(&owned_program, &Machine::default(), &policy, &Memo::new(), &Store::default())
            .unwrap();
    assert_eq!(templates(&owned), templates(&cold) + 1, "kept under their codes, two templates");

    store.keep(&cold.puts);
    let warm_machine = Machine::default();
    let warm = build(&program, &warm_machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm_machine.prepared.load(Ordering::Relaxed), 0, "both from the store");
    assert_eq!(warm.functions, cold.functions, "each restored at its own positions");
}

#[test]
fn the_front_end_hears_each_level_before_it_and_a_stop_ends_the_build() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    build(&program, &machine, &policy, &memo, &store).unwrap();
    assert_eq!(*program.reached.lock(), [vec![1], vec![2, 3], vec![4, 5]]);

    program.stop.store(true, Ordering::Relaxed);
    let stopped = Machine::default();
    let errors = build(&program, &stopped, &policy, &Memo::new(), &store).unwrap_err();
    assert!(errors.iter().all(|it| it.what.starts_with("stopped")), "{errors:?}");
    assert_eq!(stopped.prepared.load(Ordering::Relaxed), 0);
}

/// Two functions defining one symbol are refused, naming the symbol and both: the linker's
/// table of names holds one target a name, so calls by that name went to whichever was
/// given last (two panic runtimes' `__rust_panic_cleanup`, #1611). Every such symbol is
/// refused in the one pass, from the walk, before anything is prepared or compiled.
#[test]
fn symbols_defined_twice_are_refused_before_anything_compiles_naming_each() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    for (id, symbol) in [
        (4, "__rust_panic_cleanup"),
        (5, "__rust_panic_cleanup"),
        (2, "__rust_start_panic"),
        (3, "__rust_start_panic"),
    ] {
        program.functions.get_mut(&id).unwrap().symbol = Some(symbol.to_owned());
    }
    let machine = Machine::default();
    let errors = build(&program, &machine, &policy, &Memo::new(), &Store::default()).unwrap_err();
    let mut whats: Vec<&str> = errors.iter().map(|it| it.what.as_str()).collect();
    whats.sort_unstable();
    assert_eq!(
        whats,
        [
            "the symbol `__rust_panic_cleanup` is defined twice in one program, by `f4` and by `f5`",
            "the symbol `__rust_start_panic` is defined twice in one program, by `f2` and by `f3`",
        ],
        "{errors:?}"
    );
    assert_eq!(machine.prepared.load(Ordering::Relaxed), 0, "refused before anything is prepared");
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 0, "refused before anything is compiled");
}

#[test]
fn a_callee_edit_renames_its_callers_and_compiles_only_it() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    let memo = Memo::new();
    let mut store = Store::default();
    let before = build(&program, &Machine::default(), &policy, &memo, &store).unwrap();
    store.keep(&before.puts);

    program.functions.get_mut(&4).unwrap().body = 0x45;
    let machine = Machine::default();
    let after = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 1);
    let key_of = |built: &Built, body: u8| {
        built.functions.iter().find(|it| it.code[0] == body).map(|it| it.key).unwrap()
    };
    let (old, new) = (key_of(&before, 0x44), key_of(&after, 0x45));
    assert_ne!(old, new);
    // 2 is the same template; its call now names the new 4.
    let caller =
        after.functions.iter().find(|it| it.relocs.targets().contains(&Target::Direct(new)));
    assert!(caller.is_some_and(|it| it.code[0] == 0x22));
    assert!(after.functions.iter().all(|it| !it.relocs.targets().contains(&Target::Direct(old))));
}

/// A function defined weakly under a name (`#[linkage = "weak"]`, as compiler-builtins'
/// intrinsics are) is referred to by that name by every caller, never by its key: a strong
/// definition elsewhere on the link (a C library's `compiler_rt.o`) then takes every call
/// over, as ld64 and lld bind them. Its definition is still built and named, weakly.
#[test]
fn a_weakly_defined_function_is_called_by_its_name() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    let weak = program.functions.get_mut(&4).unwrap();
    weak.symbol = Some("__adddf3".to_owned());
    weak.linkage = crate::Linkage::Weak;
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default())
        .unwrap();
    let key_of = |body: u8| built.functions.iter().find(|it| it.code[0] == body).unwrap().key;
    let (weak, strong) = (key_of(0x44), key_of(0x55));
    assert!(
        built.symbols.contains(&(weak, "__adddf3".to_owned(), crate::Linkage::Weak)),
        "{:?}",
        built.symbols
    );
    let by_name = Target::Extern("__adddf3".to_owned());
    let caller = built.functions.iter().find(|it| it.relocs.targets().contains(&by_name));
    assert!(caller.is_some_and(|it| it.code[0] == 0x22), "2 calls 4 by its name");
    for function in &built.functions {
        let targets = function.relocs.targets();
        assert!(
            !targets.contains(&Target::Direct(weak)) && !targets.contains(&Target::Function(weak)),
            "{:?} calls the weak definition by key",
            function.key
        );
    }
    // A strong function is still called by key.
    assert!(built.functions.iter().any(|it| it.relocs.targets().contains(&Target::Direct(strong))));
}

#[test]
fn equal_code_is_one_function_unless_its_address_is_significant() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = Program::default();
    program.function(1, 0x11, &[2, 3]).symbol = Some("main".to_owned());
    program.function(2, 0x77, &[]);
    program.function(3, 0x77, &[]);
    let built =
        build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    assert_eq!(built.deduped.merged.len(), 1);
    assert_eq!(built.functions.len(), 2);

    program.functions.get_mut(&3).unwrap().symbol = Some("exported".to_owned());
    let built =
        build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    assert!(built.deduped.merged.is_empty());
    assert_eq!(built.functions.len(), 3);
}

#[test]
fn a_root_reaches_what_it_calls_and_an_edit_elsewhere_changes_nothing_it_reaches() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    let built =
        build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    let key =
        |built: &Built, name: &str| built.debug_names.iter().find(|it| it.1 == name).unwrap().0;
    assert_eq!(built.inputs.len(), 5);
    assert!(built.inputs.is_sorted_by_key(|it| it.0));
    assert_eq!(built.debug_names.len(), 5);
    let all = built.reached(&built.roots[0]);
    let two = built.reached(&key(&built, "f2"));
    // 2's code and 4's, and the arguments both have.
    assert_eq!(two.len(), 3);
    assert!(two.iter().all(|it| all.contains(it)) && two.len() < all.len());
    assert!(all.is_sorted());

    program.functions.get_mut(&5).unwrap().body = 0x56;
    let after =
        build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    assert_eq!(after.reached(&key(&after, "f2")), two);
    assert_ne!(after.reached(&after.roots[0]), all);
}

#[test]
fn a_static_root_is_built_though_no_code_names_it() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    program.statics.insert(7, (0xaa, None));
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let without = build(&program, &machine, &policy, &memo, &store).unwrap();
    assert!(without.statics.is_empty());
    let with = Build::new(&machine, &program, &policy, &memo)
        .run_with_statics(&[(1, member())], &[7], &[], &store)
        .unwrap();
    assert_eq!(with.statics.len(), 1);
    assert_eq!(with.statics[0].item, static_item(7));
    assert_eq!(with.functions.len(), without.functions.len());
}

#[test]
fn statics_are_kept_by_which_they_are() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    program.statics.insert(7, (0xaa, None));
    program.statics.insert(8, (0xaa, None));
    program.functions.get_mut(&4).unwrap().statics = vec![7, 8];
    let machine = Machine::default();
    let mut store = Store::default();
    let built = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    // Equal values, two statics, each compiled: a cold build reads the store before either.
    assert_eq!(built.statics.len(), 2);
    assert_eq!(machine.statics.load(Ordering::Relaxed), 2);
    let caller = built.functions.iter().find(|it| it.code[0] == 0x44).unwrap();
    let statics =
        caller.relocs.iter().filter(|it| matches!(it.target, Target::Static { .. })).count();
    assert_eq!(statics, 2);
    store.keep(&built.puts);
    let warm = Machine::default();
    let again = build(&program, &warm, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm.statics.load(Ordering::Relaxed), 0);
    assert_eq!(again.statics, built.statics);
}

#[test]
fn a_static_is_kept_with_its_section() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    program.statics.insert(7, (0xaa, None));
    program.statics.insert(8, (0xaa, Some(".init_array".to_owned())));
    program.functions.get_mut(&4).unwrap().statics = vec![7, 8];
    let mut store = Store::default();
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&built.puts);
    let warm = Machine::default();
    let again = build(&program, &warm, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm.statics.load(Ordering::Relaxed), 0);
    let sections: Vec<_> = again.statics.iter().map(|it| it.section.clone()).collect();
    let item_order = static_item(7) < static_item(8);
    let want = if item_order {
        vec![None, Some(".init_array".to_owned())]
    } else {
        vec![Some(".init_array".to_owned()), None]
    };
    assert_eq!(sections, want);
}

#[test]
fn a_release_build_summarizes_decides_and_keeps_by_imports() {
    let (program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    let (machine, mut store) = (Machine::default(), Store::default());
    let cold = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(machine.summarized.load(Ordering::Relaxed), 5);
    assert_eq!(cold.functions.len(), 5);
    assert!(cold.functions.iter().all(|it| it.arch == Arch::X86_64));
    // What the backend called of its own accord is kept by name, though no reference lists it.
    assert!(cold.functions.iter().all(|it| it.relocs.targets().contains(&bcmp())));
    store.keep(&cold.puts);

    let warm = Machine::default();
    let again = build(&program, &warm, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm.prepared.load(Ordering::Relaxed), 0);
    assert_eq!(warm.summarized.load(Ordering::Relaxed), 0);
    assert_eq!(again.functions, cold.functions);
}

/// An edit of 4 renames 4 and not its caller 2, whose key takes it by its interface: the
/// summary 2's imports are decided by is of 2 calling the new 4, never the stored one of 2
/// calling the old.
#[test]
fn a_summary_names_the_callees_its_body_calls() {
    let (mut program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    let mut store = Store::default();
    let main =
        |built: &Built| built.functions.iter().find(|it| it.code[0] == 0x11).unwrap().code.clone();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    // Built in at depth through 2: what 2's summary says it calls.
    assert!(main(&cold).contains(&0x44), "{:x?}", main(&cold));
    store.keep(&cold.puts);

    program.functions.get_mut(&4).unwrap().body = 0x45;
    let after = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    assert!(main(&after).contains(&0x45), "{:x?}", main(&after));
    assert!(!main(&after).contains(&0x44), "{:x?}", main(&after));
}

/// 4 reads a static it did not, which its code hash leaves out, as a fact of the arguments a
/// share key misses adds a call: one more reference under one instance key. Each body gets a
/// template of its own, and both are kept: neither is given the other's.
#[test]
fn a_body_with_one_more_reference_has_a_template_of_its_own() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    program.statics.insert(7, (0xaa, None));
    let mut store = Store::default();
    let without = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&without.puts);

    program.functions.get_mut(&4).unwrap().statics = vec![7];
    let machine = Machine::default();
    let with = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 1);
    let read =
        |built: &Built| built.functions.iter().any(|it| it.code[0] == 0x44 && it.relocs.len() == 2);
    assert!(read(&with));
    assert!(!read(&without));
    store.keep(&with.puts);

    program.functions.get_mut(&4).unwrap().statics = Vec::new();
    let machine = Machine::default();
    let again = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 0);
    assert_eq!(again.functions, without.functions);
}

/// 4 calls `memmove` where it called `memcpy`, which no key holds: 1 and 2, which import 4,
/// have one reference more than their stored templates were made against, as `memmove` is
/// none of theirs, so they are compiled again; 4's own template, and 3's and 5's, still fit.
#[test]
fn a_release_compile_is_kept_by_which_of_its_references_are_one() {
    let (mut program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    let mut store = Store::default();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&cold.puts);

    program.functions.get_mut(&4).unwrap().outside = "memmove";
    let machine = Machine::default();
    let after = build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(after.functions.len(), 5);
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 2);
    let memmove = Target::Extern("memmove".to_owned());
    assert!(after.functions.iter().any(|it| it.relocs.targets().contains(&memmove)));
}

#[test]
fn a_live_build_starts_at_cranelift_and_tiers_up() {
    let mut policy = policy(OptLevel::O0, Mode::Live);
    policy
        .profile
        .packages
        .insert("*".to_owned(), Override { opt_level: Some(OptLevel::O3), ..Override::default() });
    let mut program = program();
    program.functions.get_mut(&4).unwrap().origin = Origin::Dependency(Package::new("memchr"));
    program.functions.get_mut(&4).unwrap().owner = Owner::Dependency;
    let machine = Machine::default();
    let memo = Memo::new();
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run(&[(1, member())], &Store::default()).unwrap();
    assert_eq!(built.tiered.len(), 1);
    // Members and the tiered function are reached through their slots.
    let tiered = built.tiered[0];
    assert!(
        built.functions.iter().any(|it| it.relocs.targets().contains(&Target::Function(tiered)))
    );
    assert_eq!(machine.summarized.load(Ordering::Relaxed), 0);
    let up = build.tier_up(&Store::default()).unwrap();
    assert_eq!(up.replaced.len(), 1);
    assert_eq!(up.replaced[0].0, tiered);
    assert_ne!(up.replaced[0].1.key, tiered);
}

#[test]
fn a_tiered_callee_may_be_built_in_but_a_patchable_one_may_not() {
    let mut policy = policy(OptLevel::O0, Mode::Live);
    policy
        .profile
        .packages
        .insert("*".to_owned(), Override { opt_level: Some(OptLevel::O3), ..Override::default() });
    let mut program = program();
    program.functions.get_mut(&4).unwrap().origin = Origin::Dependency(Package::new("memchr"));
    program.functions.get_mut(&4).unwrap().owner = Owner::Dependency;
    program.functions.get_mut(&2).unwrap().built_in = vec![0];
    let built = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap();
    assert_eq!(built.tiered.len(), 1);

    program.functions.get_mut(&1).unwrap().built_in = vec![0];
    let errors = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].item, item(1));
    assert!(errors[0].what.contains("which is patchable"), "{}", errors[0].what);
}

#[test]
fn a_store_entry_that_does_not_read_is_an_error_not_a_compile() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let mut store = Store::default();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&cold.puts);
    for bytes in store.0.values_mut() {
        bytes.truncate(3);
    }
    let machine = Machine::default();
    let errors = build(&program, &machine, &policy, &Memo::new(), &store).unwrap_err();
    assert_eq!(errors.len(), 5);
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 0);
}

/// A read of the store hands its bytes to the decoder in place: a warm hit allocates nothing
/// the decoder does not.
#[test]
fn a_store_hit_is_read_in_place() {
    let at = |n: u8| {
        kernel::template(
            &InstanceKey(ContentId([n; 32])),
            &crate::Settings(ContentId::default()),
            &ContentId::default(),
        )
    };
    let key = at(1);
    let mut store = Store::default();
    store.0.insert(key, vec![7; 4096]);
    let unit = Unit {
        instance: Instance {
            code: CodeHash(ContentId::default()),
            args: ContentId::default(),
            settings: crate::Settings(ContentId::default()),
        },
        backend: Backend::Cranelift { speed: false },
        arch: Arch::X86_64,
        item: item(0),
    };
    let mut out = Out::default();
    let sum = |bytes: &[u8]| Ok::<u64, String>(bytes.iter().map(|&it| u64::from(it)).sum());
    assert!(matches!(out.read(&store, &key, &unit, sum), Read::Hit(28672)));
    let read = blaze_alloc_count::assert_at_most(0, "a warm store hit", || {
        out.read(&store, &key, &unit, sum)
    });
    assert!(matches!(read, Read::Hit(28672)));
    let missing = at(2);
    assert!(matches!(out.read(&store, &missing, &unit, sum), Read::Miss));
    assert!(out.errors.is_empty());
}

/// Rule 18 within one build: two puts under one key are one when their kind and bytes are
/// equal, and of another kind or other bytes the build is refused as the store refuses the
/// second, the error naming both putters, the key and both content IDs. Which put is first is
/// the same in whatever order they were pushed (rule 22).
#[test]
fn a_second_put_of_other_bytes_under_one_key_is_refused() {
    let unit = |n: u8| Unit {
        instance: Instance {
            code: CodeHash(ContentId([n; 32])),
            args: ContentId::default(),
            settings: crate::Settings(ContentId::default()),
        },
        backend: Backend::Cranelift { speed: false },
        arch: Arch::X86_64,
        item: item(u32::from(n)),
    };
    let (first, then) = (unit(1), unit(2));
    let (a, b) = (InstanceKey(ContentId([11; 32])), InstanceKey(ContentId([12; 32])));
    let (by_a, by_b) = (By::node(a, &first), By::node(b, &then));
    let key = blaze_cas::Key(ContentId([9; 32]));
    let side = keys::side(&ContentId([8; 32]));
    let names = |of: Of| match of {
        Of::Function(it) if it == a => Some("first_fn"),
        Of::Function(it) if it == b => Some("then_fn"),
        _ => None,
    };
    let finished = |puts: &[(By, Key, Kind, &[u8])]| {
        let mut out = Out::default();
        for &(by, key, kind, bytes) in puts {
            out.put(by, key, kind, bytes.to_vec());
        }
        out.finish(names)
    };

    let equal = finished(&[
        (by_a, key, Kind::Code, &[1, 2, 3]),
        (by_b, side, Kind::Debug, &[4]),
        (by_b, key, Kind::Code, &[1, 2, 3]),
    ]);
    assert!(equal.errors.is_empty(), "{:?}", equal.errors);
    let mut keys = vec![key, side];
    keys.sort();
    assert_eq!(equal.puts.iter().map(|it| it.key).collect::<Vec<_>>(), keys);

    let other = [(by_a, key, Kind::Code, &[1, 2, 3][..]), (by_b, key, Kind::Code, &[1, 2, 4][..])];
    let (forward, backward) = (finished(&other), finished(&[other[1], other[0]]));
    assert_eq!(forward.puts, backward.puts, "the push order decides nothing");
    assert_eq!(forward.errors, backward.errors, "the push order decides nothing");
    assert_eq!(forward.puts.len(), 1);
    assert_eq!(forward.errors.len(), 1, "{:?}", forward.errors);
    let refused = &forward.errors[0];
    let (kept, given) = (blaze_cas::content_of(&[1, 2, 3]), blaze_cas::content_of(&[1, 2, 4]));
    let (kept, given) = if by_a < by_b { (kept, given) } else { (given, kept) };
    let parts = [
        "`first_fn`".to_owned(),
        "`then_fn`".to_owned(),
        format!("{key:?}"),
        format!("{kept:?}"),
        format!("{given:?}"),
    ];
    for part in &parts {
        assert!(refused.what.contains(part.as_str()), "{} does not name {part}", refused.what);
    }
    assert!(refused.what.contains("refused"), "{}", refused.what);

    let kinds =
        finished(&[(by_a, key, Kind::Code, &[1, 2, 3]), (by_b, key, Kind::Debug, &[1, 2, 3])]);
    assert_eq!(
        kinds.errors.len(),
        1,
        "another kind under one key is refused too: {:?}",
        kinds.errors
    );
    assert!(
        kinds.errors[0].what.contains("Debug") && kinds.errors[0].what.contains("Code"),
        "{}",
        kinds.errors[0].what
    );
}

fn test_static(id: u32) -> DescribedStatic<u32, u32> {
    DescribedStatic {
        item: static_item(id),
        instance: ContentId::default(),
        value: ContentId::of("blaze v2 test value 1", &[id as u8]),
        mutable: false,
        thread_local: false,
        section: None,
        origin: Origin::Member(Package::new("app")),
        owner: Owner::Workspace,
        symbol: None,
        linkage: crate::Linkage::Strong,
        name: format!("app::S{id}"),
        refs: Vec::new(),
    }
}

/// A static's definition is kept under a key of how many references it has and the kind of
/// each, as a function's is ([`placed`]): a template laid out for other places is another.
#[test]
fn a_static_definition_key_covers_its_reference_count_and_kinds() {
    let described = test_static(1);
    let callee = Target::Function(InstanceKey(ContentId([2; 32])));
    let one = definition(&described, std::slice::from_ref(&callee));
    assert_ne!(definition(&described, &[]), one, "the count of references");
    assert_ne!(definition(&described, &[callee.clone(), callee]), one, "the count of references");
    assert_ne!(
        definition(&described, &[Target::Data(ContentId([3; 32]))]),
        one,
        "a reference's kind"
    );
    assert_ne!(
        definition(&described, &[Target::Direct(InstanceKey(ContentId([2; 32])))]),
        one,
        "a reference's kind"
    );
    assert_eq!(
        definition(&described, &[Target::Function(InstanceKey(ContentId([7; 32])))]),
        one,
        "which target is at a place is not covered, as a template takes any"
    );
}

/// Two statics that put other bytes under one key are each named by their path, through the
/// walk's names ([`names_of`]).
#[test]
fn a_statics_refused_put_names_it_by_its_path() {
    let spec = policy(OptLevel::O0, Mode::Output)
        .choose(&Origin::Member(Package::new("app")), None)
        .unwrap()
        .first;
    let (a, b): (StaticId, StaticId) =
        ((static_item(1), ContentId::default()), (static_item(2), ContentId::default()));
    let mut map = blaze_hash::HashMap::new_in(Global);
    for (id, at) in [(1, a), (2, b)] {
        map.insert(
            at,
            StaticNode::<Machine> {
                value: id,
                described: test_static(id),
                spec: spec.clone(),
                refs: Vec::new(),
            },
        );
    }
    let statics = Sorted::of(map);
    let nodes = Sorted::<InstanceKey, Node<Machine>>::default();
    let unit = Unit {
        instance: Instance {
            code: CodeHash(ContentId([1; 32])),
            args: ContentId::default(),
            settings: spec.settings(),
        },
        backend: spec.backend,
        arch: Arch::X86_64,
        item: static_item(1),
    };
    let key = blaze_cas::Key(ContentId([9; 32]));
    let mut out = Out::default();
    out.put(By::static_(a, &unit), key, Kind::Code, vec![1]);
    out.put(By::static_(b, &unit), key, Kind::Code, vec![2]);
    let finished = out.finish(names_of(&nodes, Some(&statics)));
    assert_eq!(finished.errors.len(), 1, "{:?}", finished.errors);
    let what = &finished.errors[0].what;
    assert!(
        what.contains("the static `app::S1`") && what.contains("the static `app::S2`"),
        "{what}"
    );
}

/// A store-keyed template the memo already keeps another of is refused, never instantiated,
/// and the build's error names the memo's and the compile's.
#[test]
fn a_store_keyed_memo_put_of_another_template_is_refused() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let cold =
        build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    // alloc: a test's templates
    let templates: Vec<Template> =
        cold.puts.iter().filter_map(|it| blob::decode(&it.bytes).ok()).collect();
    let first = templates[0].clone();
    let second =
        templates.iter().find(|it| **it != first).expect("a build of two templates").clone();
    let memo = Memo::new();
    let key = blaze_cas::Key(ContentId([9; 32]));
    let unit = Unit {
        instance: Instance {
            code: CodeHash(ContentId([1; 32])),
            args: ContentId::default(),
            settings: crate::Settings(ContentId::default()),
        },
        backend: Backend::Cranelift { speed: false },
        arch: Arch::X86_64,
        item: item(1),
    };
    let by = By::node(InstanceKey(ContentId([11; 32])), &unit);
    // An earlier build of the memo, which kept `first` under `key`.
    let mut earlier = Out::default();
    assert!(earlier.keyed(&memo, by, &unit, key, first.clone()).is_some());
    assert!(
        earlier.keyed(&memo, by, &unit, key, first).is_some(),
        "an equal template is the kept one"
    );
    assert!(earlier.finish(|_| Some("f")).errors.is_empty());
    let mut out = Out::default();
    assert!(
        out.keyed(&memo, by, &unit, key, second).is_none(),
        "another template is never instantiated"
    );
    let finished = out.finish(|_| Some("f"));
    assert_eq!(finished.errors.len(), 1, "{:?}", finished.errors);
    let what = &finished.errors[0].what;
    assert!(what.contains("the memo already kept") && what.contains("`f`"), "{what}");
}

/// A release compile whose template is another than the one the memo already keeps under
/// its blob key (an earlier build's, of other bytes) fails the build, which puts nothing.
#[test]
fn a_release_compile_of_other_bytes_than_the_memos_under_its_key_fails_the_build() {
    let (program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    let cold =
        build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    // alloc: a test's templates
    let templates: Vec<(Key, Template)> =
        cold.puts.iter().filter_map(|it| Some((it.key, blob::decode(&it.bytes).ok()?))).collect();
    assert!(templates.len() > 1, "{templates:?}");
    // The earlier build's: each key with another key's template.
    let memo = Memo::new();
    for (key, template) in &templates {
        let other =
            templates.iter().map(|it| &it.1).find(|it| *it != template).expect("two templates");
        assert!(memo.put_keyed(*key, other.clone()).is_ok());
    }
    let errors =
        build(&program, &Machine::default(), &policy, &memo, &Store::default()).unwrap_err();
    assert!(errors.iter().any(|it| it.what.contains("the memo already kept")), "{errors:?}");
    assert!(errors.iter().all(|it| it.what.contains("rule 18")), "{errors:?}");
}

/// A release compile's key covers the bodies each import builds in: an edit of one moves no
/// import's instance key, but its export, which the importer's code holds, is another.
#[test]
fn a_release_key_covers_the_bodies_its_imports_build_in() {
    let instance = Instance {
        code: CodeHash(ContentId([1; 32])),
        args: ContentId::default(),
        settings: crate::Settings(ContentId::default()),
    };
    let g = InstanceKey(ContentId([2; 32]));
    let imports = Imports { imported: vec![g], ..Imports::default() };
    let refs = [Target::Function(g)];
    let g_refs: [&[Target]; 1] = [&[]];
    let (h, edited) = ([InstanceKey(ContentId([3; 32]))], [InstanceKey(ContentId([4; 32]))]);
    let key = |built_in: &[InstanceKey]| {
        release_template_key(
            &instance,
            instance.key(),
            &[],
            &refs,
            &imports,
            &[g],
            &g_refs,
            &[built_in],
        )
        .1
    };
    assert_ne!(key(&h), key(&edited), "an edit of `h`, which `g` builds in");
    assert_ne!(key(&h), key(&[]), "`g` building `h` in or not");
    assert_eq!(key(&h), key(&h));
}

/// A release compile's key covers each import's unit as its export is built: in a live
/// build's tier-up the import is named by its first tier's key, while its export is built at
/// the next tier's settings (another opt level), so a key of the name alone would keep `f`'s
/// code made against the other export (rule 18).
#[test]
fn a_release_key_covers_its_imports_settings() {
    let instance = Instance {
        code: CodeHash(ContentId([1; 32])),
        args: ContentId::default(),
        settings: crate::Settings(ContentId::default()),
    };
    let g = InstanceKey(ContentId([2; 32]));
    let imports = Imports { imported: vec![g], ..Imports::default() };
    let refs = [Target::Function(g)];
    let g_refs: [&[Target]; 1] = [&[]];
    let g_at = |settings: u8| {
        Instance {
            code: CodeHash(ContentId([2; 32])),
            args: ContentId::default(),
            settings: crate::Settings(ContentId([settings; 32])),
        }
        .key()
    };
    let key = |unit: InstanceKey| {
        release_template_key(
            &instance,
            instance.key(),
            &[],
            &refs,
            &imports,
            &[unit],
            &g_refs,
            &[&[]],
        )
        .1
    };
    assert_ne!(
        key(g_at(2)),
        key(g_at(3)),
        "`g`'s export at opt level 2 and 3, under one walk name"
    );
    assert_eq!(key(g_at(3)), key(g_at(3)));
}

/// Two functions of one template code (one non-generic body in a library and in its `--test`
/// crate) whose place 0 the walk resolved to one's own instance and to another instance for
/// the other: the first's template holds its own code there
/// (`Ref::Own`), the second's the place (the template test of a call of itself), so their dev
/// and release keys and their memo shapes differ. Two that each call themselves
/// keep one key: their templates are one (rule 20, and name-free sharing kept).
#[test]
fn a_template_key_covers_which_place_is_the_function_itself() {
    let template = Instance {
        code: CodeHash(ContentId([1; 32])),
        args: ContentId::default(),
        settings: crate::Settings(ContentId::default()),
    };
    let (a, b, c) = (
        InstanceKey(ContentId([2; 32])),
        InstanceKey(ContentId([3; 32])),
        InstanceKey(ContentId([4; 32])),
    );
    let toolchain = ContentId([5; 32]);
    let dev = |own: InstanceKey, callee: InstanceKey| {
        dev_template_key(&template, own, &[], &[Target::Direct(callee)], &toolchain)
    };
    assert_ne!(dev(a, a), dev(b, a), "`a` calling itself, `b` calling `a`");
    assert_eq!(dev(a, a), dev(b, b), "each calling itself");
    assert_eq!(dev(b, c), dev(a, c), "each calling another");
    let release = |own: InstanceKey, refs: &[Target], imported: &[Target]| {
        let imports = Imports { imported: vec![c], ..Imports::default() };
        let imported: [&[Target]; 1] = [imported];
        release_template_key(&template, own, &[], refs, &imports, &[c], &imported, &[&[]]).1
    };
    let calls = |it: InstanceKey| [Target::Function(c), Target::Direct(it)];
    assert_ne!(release(a, &calls(a), &[]), release(b, &calls(a), &[]));
    assert_eq!(release(a, &calls(a), &[]), release(b, &calls(b), &[]));
    // An import's call back into the function is a place of the template too.
    let (own, back) = ([Target::Function(c)], |it: InstanceKey| [Target::Direct(it)]);
    assert_ne!(release(a, &own, &back(a)), release(b, &own, &back(a)), "`c` calls `a` back");
    let unit = Unit {
        instance: template,
        backend: Backend::Cranelift { speed: false },
        arch: Arch::X86_64,
        item: ItemKey(ContentId([6; 32])),
    };
    let shape = crate::backend::Shape::of(&unit, &[7]);
    assert_ne!(shape.own_place(Some(0)), shape.own_place(None));
    assert_eq!(shape.own_place(None), shape);
}

/// A stored template's function, read in place, is the one decoding it and instantiating
/// gives, and bytes that are no template are the outer error.
#[test]
fn a_stored_function_is_the_decoded_templates() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let mut store = Store::default();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&cold.puts);
    let key = InstanceKey(ContentId([1; 32]));
    let mut templates = 0;
    for bytes in store.0.values() {
        let Ok(template) = blob::decode(bytes) else { continue };
        templates += 1;
        for count in [template.refs, template.refs + 1] {
            let refs: Vec<Target> = (0..count)
                .map(|n| Target::Function(InstanceKey(ContentId([n as u8 + 2; 32]))))
                .collect();
            let made = stored_function(bytes, key, &refs, None).expect("a template");
            assert_eq!(made, template.instantiate(key, &refs));
        }
    }
    assert_eq!(templates, cold.functions.len());
    assert!(stored_function(&[0; 3], key, &[], None).is_err());
}

/// A function built with a profile it no longer matches is counted however the build has it:
/// compiled, or read back from the store.
#[test]
fn a_mismatched_profile_is_counted_compiled_or_stored() {
    let key = InstanceKey(ContentId([1; 32]));
    let function = |profile| CompiledFunction {
        key,
        arch: Arch::X86_64,
        align: 16,
        code: vec![0xc3],
        relocs: crate::Relocs::new(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile,
    };
    let mismatched = function(Some(crate::Profiled::Used { mismatched: true, applied: false }));
    let template = crate::template::Template::of(&mismatched, &[], &[]).unwrap();
    let stored = stored_function(&crate::blob::encode(&template), key, &[], None).unwrap().unwrap();
    assert_eq!(stored, mismatched);
    let mut out = Out::default();
    for it in [
        mismatched,
        stored,
        function(Some(crate::Profiled::Used { mismatched: false, applied: true })),
        function(None),
    ] {
        out.push(CodeHash(ContentId::default()), ItemKey(ContentId::default()), it);
    }
    assert_eq!(out.functions.len(), 4);
    let profile = ContentId([5; 32]);
    out.profiled(crate::settings::Pgo::Use(profile));
    let counts = super::ProfileCounts { used: 3, mismatched: 2, applied: 1 };
    assert_eq!(out.stats.profiles.into_iter().collect::<Vec<_>>(), [(profile, counts)]);
    // Built with a profile in a build of none is an error, not counts filed under no profile.
    let mut out = Out::default();
    let named = ItemKey(ContentId([2; 32]));
    out.push(
        CodeHash(ContentId::default()),
        named,
        function(Some(crate::Profiled::Used { mismatched: false, applied: true })),
    );
    out.profiled(crate::settings::Pgo::None);
    assert!(out.stats.profiles.is_empty());
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    // It names the first function built with the profile, by key and item (rule 16).
    assert_eq!((out.errors[0].unit, out.errors[0].item), (key, named), "{:?}", out.errors);
}

#[test]
fn a_level_without_its_backend_names_the_function() {
    let mut policy = policy(OptLevel::O3, Mode::Output);
    policy.toolchain.llvm = None;
    let errors = build(&program(), &Machine::default(), &policy, &Memo::new(), &Store::default())
        .unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].item, item(1));
}

mod flight;
mod folded;
mod later;
mod masks;
mod pieces;
mod stop;

/// Two instances of one key, the first a workspace's and the second a dependency's (cc's
/// `RawTable<(.., ToolFamily)>::reserve_rehash::{closure#1}` and its `RawTable<(Box<str>,
/// Arc<str>)>` one), are the dependency's: its references are the dependency's instance's,
/// each the image's, so the image never refers to the workspace's code.
#[test]
fn a_key_the_image_s_describer_shares_is_the_image_s_with_its_references() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = Program::default();
    program.function(1, 0x11, &[2, 3]).symbol = Some("main".to_owned());
    let workspace = program.function(2, 0x22, &[4]);
    workspace.code_of = Some(9);
    let dependency = program.function(3, 0x22, &[5]);
    (dependency.code_of, dependency.owner) = (Some(9), Owner::Dependency);
    program.function(4, 0x44, &[]);
    program.function(5, 0x55, &[]).owner = Owner::Dependency;
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(built.stats.instances, 4, "2 and 3 are one key");
    let key_of = |body: u8| built.functions.iter().find(|it| it.code[0] == body).map(|it| it.key);
    let (shared, callee) = (key_of(0x22).unwrap(), key_of(0x55).unwrap());
    let owner = |key| built.owners.iter().find(|it| it.0 == key).map(|it| it.1);
    assert_eq!((owner(shared), owner(callee)), (Some(Owner::Dependency), Some(Owner::Dependency)));
    let made = built.functions.iter().find(|it| it.key == shared).unwrap();
    assert!(made.relocs.targets().contains(&Target::Direct(callee)), "{:?}", made.relocs);
}

/// D2: `f2` imports `f4` at O3, and `f4` builds `f6` in (MIR inlining; `f6` itself is never
/// imported). An edit of `f6` moves no key `f2` names, but `f4`'s export, which `f2`'s code
/// holds, is another: `f2` is compiled again, never taken from the store as before the edit.
#[test]
fn an_edit_of_a_body_an_import_builds_in_compiles_its_importer_again() {
    let (mut program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    program.function(4, 0x44, &[6]).built_in = vec![0];
    program.function(6, 0x66, &[]);
    let mut store = Store::default();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    assert!(cold.stats.imported > 0, "an O3 build imports");
    store.keep(&cold.puts);

    program.functions.get_mut(&6).unwrap().body = 0x67;
    let machine = Machine::default();
    build(&program, &machine, &policy, &Memo::new(), &store).unwrap();
    let bodies = machine.emitted_bodies.lock();
    assert!(bodies.contains(&0x22), "`f2`, which imports `f4`, which builds `f6` in: {bodies:?}");
    assert!(!bodies.contains(&0x55), "`f5`, which imports nothing edited: {bodies:?}");
}

/// Another build's summary kept under the flight key this build claims a summary by, and
/// not equal to this build's, fails the build naming the function: the key misses an input
/// of the summary (rule 18), and neither is taken.
#[test]
fn another_summary_under_a_summarys_flight_key_fails_the_build() {
    let (program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    let cold =
        build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    let other: Vec<(InstanceKey, Summary)> = (cold.puts.iter())
        .filter(|it| it.kind == Kind::Query)
        .filter_map(|it| Some((InstanceKey(it.key.0), Summary::decode(&it.bytes).ok()?)))
        .map(|(flight, summary)| (flight, Summary { size: summary.size + 1, ..summary }))
        .collect();
    assert!(!other.is_empty(), "an O3 build keeps its summaries");
    let memo = Memo::new();
    assert!(memo.flights().keep_summaries(other).is_empty());
    let errors =
        build(&program, &Machine::default(), &policy, &memo, &Store::default()).unwrap_err();
    let refused = |it: &CodegenError| {
        it.what.contains("the function `f") && it.what.contains("its summary and another build's")
    };
    assert!(errors.iter().any(refused), "{errors:?}");
}

/// A fold candidate is prepared before its stored key is claimed; when another build of the
/// memo landed another shape under that key, the function is refused naming both shapes
/// (rule 18), never given the landed template.
#[test]
fn a_fold_candidate_prepared_as_another_shape_than_the_landed_fails_the_build() {
    let (program, policy) = (program(), folding_policy());
    let memo = Memo::new();
    build(&program, &Machine::default(), &policy, &memo, &Store::default()).unwrap();
    let salted = Machine { salt: 1, ..Machine::default() };
    let errors = build(&program, &salted, &policy, &memo, &Store::default()).unwrap_err();
    assert!(
        errors.iter().any(|it| it.what.contains("this build prepared it as shape")),
        "{errors:?}"
    );
    assert_eq!(salted.emitted.load(Ordering::Relaxed), 0, "nothing landed is compiled again");
}

/// A chain of `count` functions from 1, each calling the next, all of `body` and `drop_glue`.
/// Each that calls the next is one generic's code, as instances whose code reads their
/// arguments alike are; the last, which calls nothing, is other code (the base case's impl),
/// as a front end describes it: one code with one reference fewer would be one instance key
/// for two bodies, which the walk refuses (rule 18).
fn chain(count: u32, drop_glue: bool, limit: u32) -> Program {
    let mut program = Program { recursion_limit: Some(limit), ..Program::default() };
    for id in 1..=count {
        let calls: &[u32] = if id < count { &[id + 1] } else { &[] };
        program.function(id, 0x77, calls).code_of = (id < count).then_some(1);
        program.bodies.insert(id, (7, drop_glue));
    }
    program.functions.get_mut(&1).unwrap().symbol = Some("main".to_owned());
    program
}

/// rustc's recursion limit: one body repeated past the limit along one path stops the build
/// with an error naming that instance and its path's head, and what it reaches is never walked;
/// drop glue counts a quarter; one body reached by many paths, once each, is no repeat.
#[test]
fn a_body_repeated_past_the_limit_along_its_path_stops_the_build() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let run = |program: &Program| {
        Build::new(&Machine::default(), program, &policy, &Memo::new())
            .run(&[(1, member())], &Store::default())
    };
    // f1 is the body's first; f6 is its sixth along the path, a repeat of 5, past 4.
    let errors = run(&chain(30, false, 4)).unwrap_err();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].item, item(6));
    assert_eq!(
        errors[0].what,
        "f6 repeats its body 5 times on its path from f1, past 4 of f1's crate"
    );
    let deep = chain(30, false, 4);
    run(&deep).unwrap_err();
    assert!(deep.reached.lock().iter().flatten().all(|it| *it <= 6), "nothing past f6 is walked");

    // Drop glue: a repeat of 20 counts 5, past 4; 30 functions of it within 128 build.
    let errors = run(&chain(30, true, 4)).unwrap_err();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].item, item(21));
    assert!(run(&chain(30, true, 128)).is_ok());
    assert!(run(&chain(30, false, 128)).is_ok());

    // One body called by f1 thirty times over, each a repeat of one.
    let mut wide = Program { recursion_limit: Some(1), ..Program::default() };
    let callees: Vec<u32> = (2..=31).collect();
    wide.function(1, 0x11, &callees).symbol = Some("main".to_owned());
    wide.bodies.insert(1, (7, false));
    for id in callees {
        wide.function(id, 0x22, &[]);
        wide.bodies.insert(id, (7, false));
    }
    assert!(run(&wide).is_ok());
}

/// Which crate's limit: a path is held to its head's crate's, and a function of another crate
/// that is not generic and not `#[inline]` heads a path of its own, held to the larger limit, as
/// rustc collects it in that crate's own build.
#[test]
fn a_dependency_is_held_to_its_own_limit_where_rustc_collects_it_in_its_own_build() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let run = |program: &Program| {
        Build::new(&Machine::default(), program, &policy, &Memo::new())
            .run(&[(1, member())], &Store::default())
    };
    // `main` (crate 0, the default 128) calls `d::run` (crate 1, 256, not generic), which runs a
    // chain of 200 repeats of one generic body of crate 1: d's build collects it, within 256.
    let mut program = chain(202, false, 128);
    program.bodies.remove(&1);
    program.bodies.remove(&2);
    program.crates.insert(2, (1, true));
    program.limits.insert(1, 256);
    for id in 3..=202 {
        program.crates.insert(id, (1, false));
    }
    assert!(run(&program).is_ok(), "d's chain of 200 under d's limit of 256");
    // The same `d::run` were it generic or `#[inline]`: the bin collects it, under 128.
    program.crates.insert(2, (1, false));
    let errors = run(&program).unwrap_err();
    assert_eq!(
        errors[0].what,
        "f132 repeats its body 129 times on its path from f1, past 128 of f1's crate"
    );

    // A generic chain of a dependency with a limit of 8, reached from the root's code (limit
    // 128): the root's build collects it, held to the root's limit.
    let mut generic = chain(30, false, 128);
    generic.limits.insert(1, 8);
    for id in 2..=30 {
        generic.crates.insert(id, (1, false));
    }
    assert!(run(&generic).is_ok(), "held to the root's 128, not the dependency's 8");
    generic.recursion_limit = Some(8);
    assert!(run(&generic).is_err(), "the root's own limit of 8 holds it");
}

/// rustc's path, not the shortest: the walk checks the limit depth first in root and reference
/// order, claiming each instance's references as it lists them, never on the path it described
/// an instance by first. The reviewer's typenum shape:
/// `trait Nat { fn go(); } impl<N: Nat> Nat for S<N> { fn go() { N::go() } }`,
/// `impl Pad for Z { fn pad() { <S60 as Nat>::go() } } impl<N: Pad> Pad for S<N> { fn pad() { N::pad() } }`,
/// `fn main() { <S75 as Pad>::pad(); <S130 as Nat>::go(); }`. rustc claims `S60..S1::go` on the
/// `pad` path (depth 0 to 59), and the `go` chain from `S130` stops at `S60` at depth 69: it
/// builds. Level by level, `S60::go` is reached first by the `go` chain, and `S1::go` is 129 deep.
#[test]
fn the_limit_counts_on_rustcs_depth_first_path_not_the_shortest() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let (pad, go) = (|k: u32| 1000 + k, |k: u32| 2000 + k);
    let mut program = Program { recursion_limit: Some(128), ..Program::default() };
    program.function(1, 0x11, &[pad(75), go(130)]).symbol = Some("main".to_owned());
    for k in 0..=75 {
        let calls = if k == 0 { [go(60)] } else { [pad(k - 1)] };
        // `Z`'s impl (`k == 0`) is its own code; each `S<N>`'s is the one generic's.
        program.function(pad(k), 0x22, &calls).code_of = (k != 0).then_some(1);
        program.bodies.insert(pad(k), (if k == 0 { 3 } else { 2 }, false));
    }
    for k in 0..=130 {
        let calls: &[u32] = if k == 0 { &[] } else { &[go(k - 1)] };
        program.function(go(k), 0x33, calls).code_of = (k != 0).then_some(1);
        program.bodies.insert(go(k), (if k == 0 { 5 } else { 4 }, false));
    }
    let built = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default());
    assert!(built.is_ok(), "rustc builds it: {:?}", built.err());

    // Past the limit on rustc's path too (`S200::go` from `main` first): refused.
    for k in 131..=200 {
        program.function(go(k), 0x33, &[go(k - 1)]).code_of = Some(1);
        program.bodies.insert(go(k), (4, false));
    }
    program.function(1, 0x11, &[go(200), pad(75)]);
    let errors = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap_err();
    assert_eq!(errors[0].item, item(go(71)), "{errors:?}");
}

/// rustc's root order for the check (`collect_roots`): free items in definition order, every
/// static among them, `lang_start` last. The reviewer's program, with limit 128:
/// `#[used] static WARM: fn() = <S60 as Nat>::go; fn main() { <S130 as Nat>::go(); }`. rustc
/// visits `WARM` first and claims `S60..S1::go` at depths 0 to 59; `S130::go` then stops at
/// `S60` at depth 69: it builds. From the C `main` first, `S1::go` is 129 deep. A static no
/// code names and no linker keeps is a root of rustc's all the same: described for what it
/// refers to, never built, nor what only it reaches.
#[test]
fn a_static_defined_before_main_is_walked_first_as_rustc_does() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let go = |k: u32| 2000 + k;
    let (start, main, warm) = (1, 2, 3);
    let mut program = Program { recursion_limit: Some(128), ..Program::default() };
    program.function(start, 0x11, &[main]).symbol = Some("main".to_owned());
    program.function(main, 0x12, &[go(130)]);
    program.function(warm, 0x13, &[go(60)]);
    for k in 0..=130 {
        let calls: &[u32] = if k == 0 { &[] } else { &[go(k - 1)] };
        program.function(go(k), 0x33, calls);
        program.bodies.insert(go(k), (if k == 0 { 5 } else { 4 }, false));
    }
    let warm_static = 1;
    program.statics.insert(warm_static, (0x01, None));
    program.static_calls.insert(warm_static, vec![go(60)]);
    let rustcs = [
        CheckRoot::Static(warm_static),
        CheckRoot::Function(main, member()),
        CheckRoot::Function(start, member()),
    ];
    let run = |statics: &[u32], check: &[CheckRoot<u32, u32>]| {
        Build::new(&Machine::default(), &program, &policy, &Memo::new()).run_with_statics(
            &[(start, member())],
            statics,
            check,
            &Store::default(),
        )
    };
    // `#[used]`: a root of the build too.
    let built = run(&[warm_static], &rustcs);
    assert!(built.is_ok(), "rustc builds it: {:?}", built.err());
    // The build's order, the C `main` first: refused at `S1::go`, 129 deep.
    let errors = run(&[warm_static], &[]).unwrap_err();
    assert_eq!(errors[0].item, item(go(1)), "{errors:?}");

    // A plain static: walked by the check, never built.
    let built = run(&[], &rustcs).expect("rustc builds it");
    assert_eq!(built.stats.statics, 0, "nothing names `WARM`: it is not built");
    assert_eq!(built.stats.instances, 133, "the C `main`, `main` and `S130..S0::go`");

    // A static of a function nothing else reaches: the check walks it, the build leaves both out.
    program.static_calls.insert(warm_static, vec![warm]);
    let built = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run_with_statics(&[(start, member())], &[], &rustcs, &Store::default())
        .expect("rustc builds it");
    assert_eq!(built.stats.statics, 0);
    assert_eq!(built.stats.instances, 133, "`warm`, reached only from `WARM`, is not built");
}

/// Only the check's last pass errs: an instance the first pass places past the limit, because
/// the instance whose path claims it first in rustc's order was deferred by the levels and not
/// yet described, is claimed within the limit once that one is, and the build succeeds.
/// Limit 2; `main` calls `a1` then `x0`; `a1 → a2 → a3 → a4 → d` (none counted); `x0 → x1 →
/// x2 → [d, y]`, `d → y`, the `x`s, `d` and `y` one body. The levels reach `d` and `y` through
/// `x2` first, 3 deep, and defer both. The first check stops at `d` on the `a` path and places
/// `y` 3 deep on the `x` path, past 2; once `d` is described, the `a` path claims `y` 1 deep.
#[test]
fn a_refusal_of_an_early_pass_is_gone_once_the_deferred_are_described() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let (main, a, x, d, y) = (1, |k: u32| 10 + k, |k: u32| 20 + k, 30, 31);
    let mut program = Program { recursion_limit: Some(2), ..Program::default() };
    program.function(main, 0x11, &[a(1), x(0)]).symbol = Some("main".to_owned());
    for k in 1..=4 {
        let next = if k == 4 { d } else { a(k + 1) };
        program.function(a(k), 0x20 + k as u8, &[next]);
    }
    program.function(x(0), 0x30, &[x(1)]);
    program.function(x(1), 0x31, &[x(2)]);
    program.function(x(2), 0x32, &[d, y]);
    program.function(d, 0x33, &[y]);
    program.function(y, 0x34, &[]);
    for id in [x(0), x(1), x(2), d, y] {
        program.bodies.insert(id, (7, false));
    }
    let run = |program: &Program| {
        Build::new(&Machine::default(), program, &policy, &Memo::new())
            .run(&[(main, member())], &Store::default())
    };
    let built = run(&program).expect("the last pass refuses nothing");
    assert_eq!(built.stats.instances, 10);
    // Without the `a` path, `d` and `y` are 3 deep on rustc's path too: refused.
    program.function(main, 0x11, &[x(0)]);
    let errors = run(&program).unwrap_err();
    let mut refused: Vec<ItemKey> = errors.iter().map(|it| it.item).collect();
    refused.sort_unstable();
    let mut want = vec![item(d), item(y)];
    want.sort_unstable();
    assert_eq!(refused, want, "{errors:?}");
}

/// A release compile that says it built in a code none of its own bodies, its imports or
/// theirs is fails the build naming the function, the code and its template's key, never
/// keeps the template under a key that misses that body (rule 18).
#[test]
fn a_release_compile_that_built_in_unkept_code_fails_the_build() {
    let (mut program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    program.function(4, 0x44, &[6]).built_in = vec![0];
    program.function(6, 0x66, &[]);
    // Only `f2` is of the body the knob strays.
    program.functions.get_mut(&3).unwrap().body = 0x33;
    // As D2 builds it, each import's code and what it builds in is kept: `f2` imports `f4`,
    // whose export builds `f6` in.
    let plain =
        build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    assert!(plain.stats.imported > 0, "`f2` imports `f4`");
    let stray = CodeHash(ContentId([0x77; 32]));
    let machine = Machine { unkept: Some((0x22, stray)), ..Machine::default() };
    let errors = build(&program, &machine, &policy, &Memo::new(), &Store::default()).unwrap_err();
    let named = |it: &CodegenError| {
        it.what.contains("`f2` built in the code")
            && it.what.contains(&format!("{stray:?}"))
            && it.what.contains("template's key")
    };
    assert!(errors.iter().any(named), "{errors:?}");
    assert!(errors.iter().all(|it| !it.what.contains("`f3`")), "{errors:?}");
}

/// The store check compiles a release template the store gave again, with its imports'
/// exports: one that built in code its key does not hold is refused there too, named as the
/// store check's, never compared with the stored bytes as if it were the same compile.
#[test]
fn the_release_store_check_refuses_code_its_key_does_not_hold() {
    let (mut program, policy) = (program(), policy(OptLevel::O3, Mode::Output));
    program.functions.get_mut(&3).unwrap().body = 0x33;
    let mut store = Store::default();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    store.keep(&cold.puts);
    let stray = CodeHash(ContentId([0x77; 32]));
    let machine = Machine { unkept: Some((0x22, stray)), ..Machine::default() };
    let memo = Memo::new();
    let mut checked = Build::new(&machine, &program, &policy, &memo)
        .verify_store(blaze_store_check::Setting::ALL);
    let errors = checked.run(&[(1, member())], &store).unwrap_err();
    let named = |it: &CodegenError| {
        it.what.starts_with(blaze_store_check::VAR)
            && it.what.contains("`f2` built in the code")
            && it.what.contains(&format!("{stray:?}"))
    };
    assert!(errors.iter().any(named), "{errors:?}");
}

/// In a whole live build's tier-up an import is named by its first tier, which an edit of
/// its own opt level leaves as it was; its export is built at the new level. `f2`, which
/// imports `f4`, is compiled again, never taken from the store as made against `f4`'s other
/// export (rule 18).
#[test]
fn a_tier_up_recompiles_its_importer_when_an_imports_tier_changes() {
    let live = |memchr: Option<OptLevel>| {
        let mut policy = policy(OptLevel::O0, Mode::Live);
        let at = |level| Override { opt_level: Some(level), ..Override::default() };
        policy.profile.packages.insert("*".to_owned(), at(OptLevel::O3));
        if let Some(level) = memchr {
            policy.profile.packages.insert("memchr".to_owned(), at(level));
        }
        policy
    };
    let mut program = program();
    program.functions.get_mut(&2).unwrap().origin = Origin::Dependency(Package::new("dep"));
    program.functions.get_mut(&2).unwrap().owner = Owner::Dependency;
    program.functions.get_mut(&4).unwrap().origin = Origin::Dependency(Package::new("memchr"));
    program.functions.get_mut(&4).unwrap().owner = Owner::Dependency;
    let mut store = Store::default();
    let (machine, memo, policy) = (Machine::default(), Memo::new(), live(None));
    let mut first = Build::new(&machine, &program, &policy, &memo);
    store.keep(&first.run(&[(1, member())], &store).unwrap().puts);
    machine.emitted_bodies.lock().clear();
    let up = first.tier_up(&store).unwrap();
    assert_eq!(up.replaced.len(), 2, "`f2` and `f4` tier up");
    let mut tiered = machine.emitted_bodies.lock().clone();
    tiered.sort_unstable();
    assert_eq!(tiered, [0x22, 0x44], "the tier-up compiles `f2` and `f4`, nothing else");
    assert!(up.stats.imported > 0, "`f2` imports `f4`");
    store.keep(&up.puts);

    let (machine, memo, policy) = (Machine::default(), Memo::new(), live(Some(OptLevel::O2)));
    let mut next = Build::new(&machine, &program, &policy, &memo);
    next.run(&[(1, member())], &store).unwrap();
    assert!(machine.emitted_bodies.lock().is_empty(), "the first tier is as it was");
    let again = next.tier_up(&store).unwrap();
    assert!(again.stats.imported > 0, "`f2` still imports `f4`: its decision is as it was");
    let bodies = machine.emitted_bodies.lock();
    assert!(bodies.contains(&0x44), "`f4`, at O2 now: {bodies:?}");
    assert!(bodies.contains(&0x22), "`f2`, whose import `f4` is built at O2 now: {bodies:?}");
}

/// One generic instance asked for by a member and by a dependency at one opt-level: one
/// settings, but two keys, as its arguments and code are each asker's (`arrayvec`'s
/// `CapacityError<&[u8; 1024]>::new` in `blaze-profile`'s tests). Its test binary's table has
/// it as two functions, each with its own ID, rather than refusing them as one function
/// across versions.
#[test]
fn one_instance_asked_for_by_two_origins_is_two_functions_of_its_test_binary() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = Program::default();
    program.function(1, 0x11, &[2, 3]).symbol = Some("main".to_owned());
    program.function(2, 0x22, &[]).identity_of = Some(9);
    let dependency = program.function(3, 0x23, &[]);
    (dependency.identity_of, dependency.origin) =
        (Some(9), Origin::Dependency(Package::new("dep")));
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(built.stats.instances, 3, "2 and 3 are two keys");
    let key_of =
        |body: u8| built.functions.iter().find(|it| it.code[0] == body).map(|it| it.key).unwrap();
    let id = |key| {
        built.inputs.binary_search_by_key(&key, |it| it.0).ok().map(|at| built.versions[at].0)
    };
    let (member, dependency) = (key_of(0x22), key_of(0x23));
    assert_ne!(id(member), id(dependency), "two functions across versions");
    let live = |key| built.identities.iter().find(|it| it.0 == key).map(|it| it.1);
    assert!(live(member).is_some() && live(dependency).is_some(), "{:?}", built.identities);
    assert_ne!(
        live(member),
        live(dependency),
        "two functions across edits: each its own live slot"
    );
    crate::ExecTable::new(&built, &[], 0, |_| None, ContentId::default(), std::iter::empty())
        .unwrap_or_else(|why| panic!("{}", why.describe(&built)));
}

/// One instance whose two askers' describes give one key is one function across versions
/// and across edits,
/// with one ID whichever asker's describe the walk kept: the least origin's, never the first
/// described (rule 22).
#[test]
fn one_key_of_two_origins_has_one_id_whichever_was_described_first() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let id_of_shared = |calls: &[u32]| {
        let mut program = Program::default();
        program.function(1, 0x11, calls).symbol = Some("main".to_owned());
        let member = program.function(2, 0x22, &[]);
        (member.identity_of, member.code_of) = (Some(9), Some(9));
        let dependency = program.function(3, 0x22, &[]);
        (dependency.identity_of, dependency.code_of) = (Some(9), Some(9));
        dependency.origin = Origin::Dependency(Package::new("dep"));
        let built = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default())
            .unwrap_or_else(|errors| panic!("{errors:?}"));
        assert_eq!(built.stats.instances, 2, "2 and 3 are one key");
        let shared = built.functions.iter().find(|it| it.code[0] == 0x22).map(|it| it.key).unwrap();
        let at = built.inputs.binary_search_by_key(&shared, |it| it.0).unwrap();
        let live = built.identities.iter().find(|it| it.0 == shared).map(|it| it.1);
        (shared, built.versions[at].0, live)
    };
    let (member_first, dependency_first) = (id_of_shared(&[2, 3]), id_of_shared(&[3, 2]));
    assert_eq!(member_first.0, dependency_first.0, "one key either way");
    assert_eq!(member_first.1, dependency_first.1, "one ID either way");
    assert!(member_first.2.is_some());
    assert_eq!(member_first.2, dependency_first.2, "one live identity either way");
}
