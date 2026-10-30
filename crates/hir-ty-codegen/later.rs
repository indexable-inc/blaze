//! What is made of a function once its code is in the object: what a debugger is told of
//! it and its entry of the store. Neither changes the object, so a build that puts the
//! functions in the object one at a time makes them for all of them at once, on every
//! core ([`ObjectCodegen::make_later`]): made as each function was put in, they took the
//! thread that does that as long as the putting in itself.

use super::*;

/// What a function put in the object is.
pub(super) enum Of {
    /// A function without generic arguments, under its rustc symbol.
    Root {
        func: FunctionId,
        symbol: String,
    },
    Instance {
        instance: Instance,
        env: StoredParamEnvAndCrate,
    },
}

/// A function whose code is in the object, with what is still to be made of it.
pub(super) struct Later<'c> {
    pub(super) id: FuncId,
    pub(super) of: Of,
    pub(super) compiled: &'c CompiledFunction,
    pub(super) ids: Resolved,
    /// Of each function and each data object the code refers to, whether the object linked
    /// to it when the code was put in: one of them can be defined in the object after.
    pub(super) imported: (Vec<bool>, Vec<bool>),
    /// The rows of a unit's code, which are not those of the query.
    pub(super) unit_rows: Option<Vec<Row>>,
    /// Code of a level above 1, which is of a profile: no key of the store, and told of by
    /// the profile.
    pub(super) tiered: bool,
    /// Whether it is given to the store.
    pub(super) keeps: bool,
}

impl<'c> Later<'c> {
    pub(super) fn new(
        module: &ObjectModule,
        id: FuncId,
        of: Of,
        compiled: &'c CompiledFunction,
        ids: Resolved,
    ) -> Self {
        let declarations = module.declarations();
        let funcs = ids.funcs.iter().map(|&it| declarations.get_function_decl(it).linkage);
        let datas = ids.datas.iter().map(|&it| declarations.get_data_decl(it).linkage);
        let imported = (
            funcs.map(|it| it == Linkage::Import).collect(),
            datas.map(|it| it == Linkage::Import).collect(),
        );
        Later { id, of, compiled, ids, imported, unit_rows: None, tiered: false, keeps: false }
    }
}

/// What was made of a [`Later`].
pub(super) struct Made {
    /// What a debugger is told of the function.
    pub(super) debug: Option<FunctionDebug>,
    /// What the store is given of it.
    pub(super) kept: Option<cached::Kept>,
}

/// What a [`Later`] is made with: what of the object it reads, and its own names.
pub(super) struct Keeper<'k, 'db> {
    pub(super) db: &'db dyn HirDatabase,
    pub(super) symbols: &'k dyn SymbolResolver,
    pub(super) names: &'k stable::StableNames<'k, 'db>,
    pub(super) stable_names: &'k mut FxHashMap<Instance, Option<StableName>>,
    pub(super) source_paths: &'k mut FxHashMap<base_db::FileId, Option<String>>,
    pub(super) kept: &'k mut Kept,
    pub(super) declarations: &'k cranelift_module::ModuleDeclarations,
    pub(super) type_ids: &'k FxHashMap<FuncId, u128>,
    pub(super) root_names: &'k FxHashMap<StableName, Option<FunctionId>>,
    pub(super) own_crates: &'k FxHashSet<Crate>,
    pub(super) told: &'k ToldTypes,
    /// Whether a debugger is told of the functions, and of their variables.
    pub(super) debug: bool,
    pub(super) variables: bool,
    /// The program the entries are of (see [`cached::Entry::key`]): `Some(None)` for a
    /// program with code of rustc's that has no identity, whose entries are not kept.
    pub(super) program: Option<Option<[u8; 32]>>,
    /// Whether what is made is kept at all: not for a patch, which refers to its base.
    pub(super) own: bool,
    /// Whether an entry of the checkout is marked for an edit to be told from it by: not
    /// for a unit's code, nor in a program with code of rustc's.
    pub(super) markable: bool,
    /// The instance each callee of the [`Later`] is as the crate rustc had compile it does
    /// ([`ObjectCodegen::settled`]), in the order of its requests; `None` when every
    /// callee is as it is asked for.
    pub(super) settled: Option<Vec<Option<Instance>>>,
}

impl<'db> Keeper<'_, 'db> {
    pub(super) fn make(&mut self, later: &Later<'_>) -> Made {
        let db = self.db;
        let (debug, declared, rows, kept) = match &later.of {
            Of::Root { func, symbol } => {
                let name = hir_ty_mir::mir::type_name::rustc_debug_function(db, *func, &[])
                    .unwrap_or_else(|| FunctionSignature::of(db, *func).name.as_str().to_owned());
                let declared = self.debug_declaration_at(*func);
                let rows = match &later.unit_rows {
                    Some(rows) => Some(rows),
                    None => (self.debug && !later.tiered).then(|| root_rows(db, *func)),
                };
                let kept = later.keeps.then(|| (root_instance(db, *func), func.krate(db)));
                (FunctionDebug::new(name, symbol.clone()), declared, rows, kept)
            }
            Of::Instance { instance, env } => {
                let name = backtrace_name(db, instance);
                let declared = self.written_at(instance, env.krate);
                let known = match instance.def {
                    InstanceDef::Fn(func) => hir_ty_mir::mir::type_name::rustc_debug_function(
                        db,
                        func,
                        instance.args.as_ref().as_slice(),
                    ),
                    InstanceDef::Closure(id) => {
                        let at = InternedClosureId::from_id(id).loc(db);
                        debug_closure(db, instance, at.owner, at.expr)
                    }
                    InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                        let at = InternedCoroutineId::from_id(id).loc(db);
                        debug_closure(db, instance, at.owner, at.expr)
                    }
                    _ => None,
                };
                let known = known.unwrap_or_else(|| name.clone());
                let rows = match &later.unit_rows {
                    Some(rows) => Some(rows),
                    None => (self.debug && !later.tiered)
                        .then(|| instance_rows(db, instance.clone(), env.clone())),
                };
                let kept = later.keeps.then(|| (instance.clone(), env.krate));
                (FunctionDebug::new(known, name), declared, rows, kept)
            }
        };
        let debug = self.describe(later.compiled, rows, debug, declared);
        let kept = kept.and_then(|(instance, home)| {
            let tally = backend::Tally::global();
            tally.step(Step::StoreWrite, || self.keep(&instance, home, later, debug.clone()))
        });
        Made { debug, kept }
    }

    fn stable_name(&mut self, instance: &Instance) -> Option<StableName> {
        if let Some(known) = self.stable_names.get(instance) {
            return known.clone();
        }
        let name = self.names.instance(instance).map(StableName::from);
        self.stable_names.insert(instance.clone(), name.clone());
        name
    }

    fn source_path(&mut self, file: base_db::FileId) -> Option<String> {
        let symbols = self.symbols;
        let spelled = || symbols.source_path(file).map(|it| as_rustc_spells(&it));
        self.source_paths.entry(file).or_insert_with(spelled).clone()
    }

    fn location_at(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> Option<(String, u32, u32)> {
        at.and_then(|(owner, span)| source_location(self.db, owner, span))
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    fn declaration_at(&mut self, func: FunctionId) -> Option<(String, u32, u32)> {
        declaration_location(self.db, func)
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    fn debug_declaration_at(&mut self, func: FunctionId) -> Option<(String, u32, u32)> {
        debug_declaration_location(self.db, func)
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    fn text(&mut self, parts: &[TextPart]) -> Option<String> {
        parts
            .iter()
            .map(|part| match part {
                TextPart::Text(text) => Some(text.clone()),
                TextPart::FilePath(file) => self.source_path(*file),
            })
            .collect()
    }

    /// Where what `instance` is the code of is written, which is the line of whatever of its
    /// code is of no statement: a function where it is declared, a closure or an `async`
    /// block where it stands, and a shim where the function it stands in for is declared, as
    /// rustc has it.
    fn written_at(&mut self, instance: &Instance, krate: Crate) -> Option<(String, u32, u32)> {
        if !self.debug {
            return None;
        }
        let db = self.db;
        let lang_items = DbInterner::new_with(db, krate).lang_items();
        let ty = || instance.args.as_ref().as_slice().first().and_then(|arg| arg.ty());
        let of_expr = |owner: InferBodyId<'db>, expr| match owner {
            InferBodyId::DefWithBodyId(owner) => Some((owner, MirSpan::ExprId(expr))),
            _ => None,
        };
        let func = match instance.def {
            InstanceDef::Fn(func) => Some(func),
            InstanceDef::Closure(id) => {
                let at = InternedClosureId::from_id(id).loc(db);
                return self.location_at(of_expr(at.owner, at.expr));
            }
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                let at = InternedCoroutineId::from_id(id).loc(db);
                return self.location_at(of_expr(at.owner, at.expr));
            }
            InstanceDef::DropGlue => lang_items.DropInPlace.or(lang_items.DropGlue),
            InstanceDef::CallOnceShim => lang_items.FnOnce_call_once,
            InstanceDef::VtableShim(func) => Some(func),
            InstanceDef::FnPointerShim => match ty()?.kind() {
                TyKind::Closure(id, _) => {
                    let at = id.0.loc(db);
                    return self.location_at(of_expr(at.owner, at.expr));
                }
                _ => None,
            },
            InstanceDef::ReifyShim { .. } => match ty()?.kind() {
                TyKind::FnDef(def, _) => match def.0 {
                    CallableDefId::FunctionId(func) => Some(func),
                    constructor => {
                        return constructor_location(db, constructor).and_then(
                            |(file, line, col)| Some((self.source_path(file)?, line, col)),
                        );
                    }
                },
                _ => lang_items.FnOnce_call_once,
            },
        };
        self.debug_declaration_at(func?)
    }

    /// What a debugger is told of the compiled function: what it is called, as `debug` has
    /// it, and which line each of its instructions is of, as `rows` have it. `None` when no
    /// debugger is told anything, which is when there are no `rows`.
    fn describe(
        &mut self,
        compiled: &CompiledFunction,
        rows: Option<&Vec<Row>>,
        mut debug: FunctionDebug,
        declared: Option<(String, u32, u32)>,
    ) -> Option<FunctionDebug> {
        let rows = rows?;
        let _span = tracing::info_span!(target: "blaze", "describe").entered();
        if let Some((file, line, column)) = declared {
            debug.declared = Some(debug.place(file, line, column));
        }
        for &(offset, file, line, column) in rows {
            let Some(file) = self.source_path(file) else { continue };
            let place = debug.place(file, line, column);
            debug.row(offset, place);
        }
        // Compiled code of no statement with a place is of where the function is written
        // throughout. A function that is its assembly has no lines, as rustc gives it none.
        if let Some(declared) = debug.declared
            && debug.rows.is_empty()
            && compiled.code.unwind.is_some()
        {
            debug.row(0, declared);
        }
        if let Some((owner, _)) = &compiled.source
            && let Some((env, variables)) = &compiled.variables
            && self.debug
            && self.variables
        {
            let places = &compiled.code.slots;
            let db = self.db;
            let len = compiled.code.bytes.len() as u32;
            let mut rows: Vec<(u32, MirSpan)> = match &compiled.source {
                Some((_, spans)) => (compiled.code.lines.iter())
                    .filter_map(|row| Some((row.offset, *spans.get(row.span as usize)?)))
                    .collect(),
                None => Vec::new(),
            };
            rows.sort_by_key(|row| row.0);
            let scopes = describe_scopes(db, *owner, variables, &rows, len, &mut debug);
            describe_variables(
                db,
                *owner,
                (self.told, env),
                variables,
                places,
                &scopes,
                &mut debug,
            );
        }
        Some(debug)
    }

    /// What the store is given of a stitched instance, when everything it is and refers to
    /// has a name that another run of the daemon can find it by. With `BLAZE_CACHE_CHECK`
    /// the entry is set against the one the store has here, and nothing is given.
    fn keep(
        &mut self,
        instance: &Instance,
        home: Crate,
        later: &Later<'_>,
        debug: Option<FunctionDebug>,
    ) -> Option<cached::Kept> {
        let store = store::Store::global().filter(|_| self.own)?;
        let Some(name) = self.stable_name(instance) else {
            self.kept.unnamed += 1;
            return None;
        };
        let told = cached::Told::of(debug.is_some(), self.variables);
        let key = cached::Entry::key(&name, told, self.program.flatten().as_ref())?;
        let check = std::env::var_os("BLAZE_CACHE_CHECK");
        let sets = || debug.iter().flat_map(|it| &it.types).cloned().collect::<Vec<_>>();
        if check.is_none() && store.has(&key) {
            return Some(cached::Kept::Types(sets()));
        }
        let described = debug.is_some();
        let Some(mut entry) = self.entry(instance, later, debug) else {
            self.kept.unnamed_requests += 1;
            return None;
        };
        if self.markable && stable::StableNames::of_checkout(&name) {
            match self.tracked(instance, home, later.compiled, described) {
                Ok(tracked) => entry.tracked = Some(tracked),
                Err(why) => self.kept.untracked(&name, why),
            }
        }
        let mut entry = Box::new(entry);
        let Some(check) = check else {
            return Some(cached::Kept::Entry { key, entry });
        };
        // The entry as it would be written is set against the one that is there.
        let stored = store.get(&key);
        let mut alone =
            cached::Writer::alone(store, self.symbols.cargo_home(), self.symbols.library_sources());
        let Some(bytes) = alone.encoded(&mut entry) else {
            return self.kept.unkept(OF_CARGO);
        };
        match stored {
            None => {
                store.insert(&key, &bytes);
                self.kept.added += 1;
            }
            Some(stored) if stored == bytes => self.kept.same += 1,
            Some(stored) => {
                self.kept.differing += 1;
                eprintln!("entry of `{name}` is not what was compiled");
                if check != "1" {
                    let at = std::path::Path::new(&check).join(self.kept.differing.to_string());
                    let was = cached::Entry::decode(&stored);
                    _ = std::fs::write(at, format!("{name}\n{was:#?}\n{entry:#?}"));
                }
            }
        }
        None
    }

    /// What a patch base that has the code of `instance` from the store tells an edit of it by.
    /// `None` for code the base has to compile to tell: in a program with code of rustc's,
    /// which settles what an instance is by its build, or of a `#[blaze::cache]` function,
    /// whose code holds a hash of everything it reaches.
    fn tracked(
        &self,
        instance: &Instance,
        home: Crate,
        compiled: &CompiledFunction,
        described: bool,
    ) -> std::result::Result<cached::Tracked, &'static str> {
        if let InstanceDef::Fn(func) = instance.def
            && fncache::cached_closure(self.db, func).is_some()
        {
            return Err("a `#[blaze::cache]` function");
        }
        let mark = patch::stable_mark(self.db, self.names, compiled, described)
            .ok_or("what it refers to has no stable name")?;
        let files = home::body_files(self.db, instance).and_then(|files| {
            files.iter().map(|&file| self.names.file_path(file)).collect::<Option<Vec<_>>>()
        });
        let items = patch::written_items(self.db, self.names, instance, compiled, described);
        let home = match self.names.of_the_checkout(home) {
            true => Some(self.names.content_id(home).ok_or("its crate has no content id")?.0),
            false => None,
        };
        Ok(cached::Tracked { mark, home, files, items })
    }

    /// The function of the program that an instance without arguments is of, and its symbol.
    fn root_named(&self, name: &str) -> Option<(FunctionId, String)> {
        // A call names the lifetimes its callee has, which the root is named without.
        let erased = stable::StableNames::lifetimes_erased(name);
        if let Some(&Some(func)) = self.root_names.get(&*erased) {
            let symbol = self.symbols.function(func);
            // The one root of its name is what listing its crate would find again. In a
            // program with nothing of rustc's, a function without a symbol is so by what is
            // written, and no other build of its crate has one either.
            if symbol.is_some() || self.program.is_none() {
                return Some((func, symbol?));
            }
        }
        let of = self.names.functions_named(name);
        of.into_iter().find_map(|it| Some((it, self.symbols.function(it)?)))
    }

    /// The callee asked for at `at` among the requests, as the crate rustc had compile it does.
    fn settled(&self, at: usize, callee: &Instance) -> Option<Instance> {
        match &self.settled {
            Some(settled) => settled.get(at).cloned().flatten(),
            None => Some(callee.clone()),
        }
    }

    fn entry(
        &mut self,
        instance: &Instance,
        later: &Later<'_>,
        debug: Option<FunctionDebug>,
    ) -> Option<cached::Entry> {
        use cached::{CachedData, CachedFunc, CachedTarget};
        let (compiled, ids, db) = (later.compiled, &later.ids, self.db);
        let mut funcs = Vec::with_capacity(ids.funcs.len());
        // An item of an `extern` block is the symbol it links to, whoever declares it: the
        // allocator's shim is defined in the object of a program with nothing of rustc's.
        let foreign = |callee: &Instance| match callee.def {
            InstanceDef::Fn(func) => link_name(db, func).is_some(),
            _ => false,
        };
        let requests = compiled.requests.funcs.iter().zip(&ids.funcs).zip(&later.imported.0);
        for (at, ((request, &id), &imported)) in requests.enumerate() {
            let target = if let Some(&hash) = self.type_ids.get(&id) {
                CachedTarget::TypeId(hash)
            } else if imported
                || matches!(&request.target, FuncTarget::Instance(callee, _) if foreign(callee))
            {
                let decl = self.declarations.get_function_decl(id);
                // rustc's copy of a function of the checkout is of the checkout as rustc
                // built it, and uses rustc's copies of the statics.
                let symbol = decl.linkage_name(id).into_owned();
                // Checked too when it is one of an object blaze built before, the standard
                // library's unit: a build that compiles std's crates itself has no such symbol.
                let checked = self.symbols.provides(&symbol);
                let mut copied = None;
                let callee = match &request.target {
                    FuncTarget::Instance(callee, _) if !foreign(callee) => {
                        let Some(callee) = self.settled(at, callee) else {
                            return self.kept.unkept("unsettled callee");
                        };
                        copied = match callee.def {
                            InstanceDef::Fn(func)
                                if callee.args.as_ref().iter().all(|it| it.region().is_some()) =>
                            {
                                Some(func)
                            }
                            _ => None,
                        };
                        Some(self.stable_name(&callee))
                    }
                    _ => None,
                };
                match callee {
                    // Ours to compile, though under a symbol: rustc built nothing of its crate.
                    Some(Some(name)) if !checked && stable::StableNames::of_checkout(&name) => {
                        if self.root_named(&name).is_none() {
                            return self.kept.unkept("unlisted function of the checkout");
                        }
                        CachedTarget::Root(name.as_ref().to_owned())
                    }
                    // A function of the checkout without generic arguments, which `callee`
                    // has called as rustc's copy: what decides that is in the names the entry
                    // is kept under, but for whether the program has the copy and the future
                    // a function returns, which rustc lays out its own way.
                    Some(Some(name))
                        if stable::StableNames::of_checkout(&name)
                            && copied.is_some_and(|func| !returns_async_block(db, func))
                            && self.root_named(&name).is_some() =>
                    {
                        CachedTarget::Copy { name: name.as_ref().to_owned(), symbol }
                    }
                    Some(name)
                        if name.as_ref().is_none_or(|it| stable::StableNames::of_checkout(it)) =>
                    {
                        return self.kept.unkept("rustc's copy of a function of the checkout");
                    }
                    _ => CachedTarget::Import { symbol, checked },
                }
            } else if let FuncTarget::Instance(callee, _) = &request.target {
                let Some(callee) = &self.settled(at, callee) else {
                    return self.kept.unkept("unsettled callee");
                };
                let Some(name) = self.stable_name(callee) else {
                    return self.kept.unkept("unnamed callee");
                };
                // A function the program is built from has no entry: it is compiled anew. One
                // that is never a root calls the copy it called, the store's (#242).
                if stable::StableNames::of_checkout(&name)
                    && as_its_crate_compiles(db, self.symbols, callee)
                    && self.root_named(&name).is_some()
                    && !matches!(callee.def, InstanceDef::Fn(func)
                        if rustcs_callers_pass_scalable(db, self.symbols, func))
                {
                    CachedTarget::Root(name.as_ref().to_owned())
                } else {
                    CachedTarget::Named(name.as_ref().to_owned())
                }
            } else if let FuncTarget::Asm(text) = &request.target {
                CachedTarget::Asm(text.clone())
            } else {
                return self.kept.unkept("call of no instance");
            };
            funcs.push(CachedFunc { target, sig: request.sig.clone() });
        }
        // Only a crate of the checkout is compiled with where the checkout is.
        let placed =
            self.stable_name(instance).is_some_and(|it| stable::StableNames::of_checkout(&it));
        let home = self.symbols.cargo_home();
        let library = self.symbols.library_sources();
        let mut datas = Vec::with_capacity(ids.datas.len());
        let requests = compiled.requests.datas.iter().zip(&ids.datas).zip(&later.imported.1);
        for ((request, &id), &imported) in requests {
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } = request {
                if placed && self.symbols.names_place(&blob.bytes) {
                    return self.kept.unkept("data with the place of the checkout");
                }
                if home.as_deref().is_some_and(|home| cached::holds(&blob.bytes, home)) {
                    return self.kept.unkept("data with the place of cargo's directory");
                }
                if library.as_ref().is_some_and(|it| cached::holds(&blob.bytes, &it.here)) {
                    return self.kept.unkept("data with the place of the library's sources");
                }
            }
            let mut hashed = match request {
                DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => blob.clone(),
                _ => ConstBlob::default(),
            };
            if hash_type_ids(db, self.symbols, self.own_crates, &mut hashed).is_err() {
                return self.kept.unkept("a `TypeId` not hashed");
            }
            datas.push(match request {
                DataRequest::Blob { identity, .. } => {
                    CachedData::Blob { blob: hashed, identity: *identity }
                }
                DataRequest::Vtable { ty, trait_ref, .. } => {
                    let trait_ref = trait_ref.as_ref().map(|(id, args)| (*id, args.as_ref()));
                    let Some(key) = self.names.vtable(ty.as_ref(), trait_ref) else {
                        return self.kept.unkept("unnamed vtable");
                    };
                    CachedData::Vtable { key, blob: hashed }
                }
                DataRequest::Static(of) => {
                    let decl = self.declarations.get_data_decl(id);
                    // Whether a static of the checkout is rustc's or ours is up to the build.
                    let krate = of.krate(db);
                    let of_checkout =
                        self.own_crates.contains(&krate) || krate.data(db).origin.is_local();
                    let foreign = static_link_name(db, *of).is_some();
                    if !foreign && (!imported || of_checkout) {
                        let Some(name) = self.names.static_(*of) else {
                            return self.kept.unkept("unnamed static");
                        };
                        CachedData::OwnStatic(name)
                    } else {
                        CachedData::Static {
                            symbol: decl.linkage_name(id).into_owned(),
                            thread_local: decl.tls,
                        }
                    }
                }
                DataRequest::Location(at) => CachedData::Location(self.location_at(*at)),
                DataRequest::Declaration(func) => CachedData::Location(self.declaration_at(*func)),
                DataRequest::FilePath(file) => {
                    let Some(path) = self.source_path(*file) else {
                        return self.kept.unkept("file without a path");
                    };
                    CachedData::FilePath(path)
                }
                DataRequest::Text(parts) => {
                    let Some(text) = self.text(parts) else {
                        return self.kept.unkept("file without a path");
                    };
                    CachedData::FilePath(text)
                }
            });
        }
        Some(cached::Entry {
            label: callee_name(db, instance.def).to_owned(),
            funcs,
            datas,
            code: cached::CachedCode::from(&compiled.code),
            debug,
            tracked: None,
        })
    }
}
