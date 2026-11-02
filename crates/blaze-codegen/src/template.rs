//! A compiled function with what it refers to by place: what the store keeps and the back-end
//! memo shares (#1291).
//!
//! A [`CompiledFunction`]'s relocations name other functions by [`InstanceKey`], and an
//! instance key holds the callee's code hash: a callee's body edit gives it a new key while
//! the caller's own key stays, as the caller's code hash takes the callee by its interface.
//! So what is kept under the caller's key can't name its callees. A [`Template`] names each
//! function, static and outside symbol it refers to by its place in the instance's
//! references ([`Refs`]: the list the front end makes of an instance, in the order of its
//! MIR, without compiling it), and its own data by its place among its data. A build fills
//! the places with what they are in that build ([`Template::instantiate`]): a callee edit
//! renames relocations and compiles nothing.
//!
//! Two instances whose templates are equal compile once: that is the memo up to callee
//! renaming. A template keeps constants, layouts and positions, so the sharing is exact.
//!
//! [`Target::Function`] and [`Target::Direct`] go in one kind of instruction (a direct call
//! or address), the first through the callee's jump-table stub: which one a reference is
//! changes no byte of the template, only the place's target.

use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use blaze_mem::{Frame, Tag, frame};
use serde::{Deserialize, Serialize};

use crate::{
    Arch, CompiledFunction, Covered, Data, Init, InstanceKey, Line, Profiled, Relocs, RelocsView, Static, Target,
    Unwind,
    content,
};

/// What an instance refers to, in the order its front end lists them: every function, static
/// and outside symbol, each once. Never a [`Target::Data`] or [`Target::Position`].
pub type Refs = [Target];

/// What a relocation of a template points at.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ref {
    /// The `n`th of the instance's [`Refs`].
    Place(u32),
    /// The `n`th of the template's own data.
    Data(u32),
    Position(SpanRef),
    /// The `n`th of the template's own [`Template::externs`].
    Extern(u32),
    /// The function's own SanitizerCoverage array ([`Target::Coverage`]).
    Coverage(u32),
    /// The function's own tier-up counter cells ([`Target::Counter`]): of the key it is
    /// instantiated as, so two counting instances share one template and never one counter.
    Counter,
    /// The function's own PGO counters ([`Target::ProfileCounters`]).
    ProfileCounters,
    /// The function's own code: [`Target::Direct`] of the key it is instantiated as, the
    /// offset into it in the addend (a stencil's jump into itself).
    Own,
}

/// A data object of a template: it points only at data before it, so each one's content ID
/// can be made in order.
#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: inside `Template`, which only tests copy
pub struct TemplateData {
    pub align: u32,
    pub bytes: Vec<u8>,
    pub relocs: Relocs<Ref>,
    /// [`Data::writable`]: a static's own, its ID made of the static's and its index here.
    pub writable: bool,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: tests vary a copy; the memo and the store share one by reference
pub struct Template {
    pub arch: Arch,
    pub align: u32,
    pub code: Vec<u8>,
    pub relocs: Relocs<Ref>,
    pub data: Vec<TemplateData>,
    pub inlined: Vec<CodeHash>,
    pub side: Option<ContentId>,
    pub unwind: Option<TemplateUnwind>,
    pub lines: Vec<Line>,
    /// How many references it was made against: what [`Template::instantiate`] is given.
    pub refs: u32,
    /// Outside symbols its backend calls of its own accord, which no front end lists
    /// ([`crate::backend::Emitted::extra`]): a function of its code and settings, so kept with
    /// it and never a place.
    pub externs: Vec<String>,
    /// Its SanitizerCoverage arrays and PC table, which are its own whatever fills its places.
    pub coverage: Option<Covered>,
    /// What PGO made of it: its own record and counters, whatever fills its places.
    pub profile: Option<Profiled>,
}

/// A function's [`Unwind`], its personality routine or handler by place and its
/// language-specific data area by its index among the template's data.
#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: inside `Template`, which only tests copy
pub enum TemplateUnwind {
    Dwarf {
        code_align: u32,
        data_align: i32,
        return_address: u16,
        cie: Vec<u8>,
        fde: Vec<u8>,
        personality: Option<u32>,
        lsda: Option<u32>,
    },
    Windows { info: Vec<u8>, handler: Option<u32>, lsda: Option<u32> },
}

/// A [`Template`] borrowed: from a template ([`Template::view`]) or straight from a stored blob
/// ([`crate::blob::decode_view`]), its lists in a frame. An instance's function is made of one
/// with each part copied once, into the function ([`TemplateView::instantiate`]).
pub struct TemplateView<'a> {
    pub arch: Arch,
    pub align: u32,
    pub code: &'a [u8],
    pub relocs: RelocsView<'a, Ref>,
    pub data: &'a [DataView<'a>],
    pub inlined: &'a [CodeHash],
    pub side: Option<ContentId>,
    pub unwind: Option<UnwindView<'a>>,
    pub lines: &'a [Line],
    pub refs: u32,
    pub externs: &'a [&'a str],
    /// Owned: the function made of the view takes it as it is.
    pub coverage: Option<Covered>,
    pub profile: Option<Profiled>,
}

/// A [`TemplateData`] borrowed.
#[derive(Clone, Copy)]
pub struct DataView<'a> {
    pub align: u32,
    pub bytes: &'a [u8],
    pub relocs: RelocsView<'a, Ref>,
    pub writable: bool,
}

/// A [`TemplateUnwind`] borrowed.
#[derive(Clone, Copy)]
pub enum UnwindView<'a> {
    Dwarf {
        code_align: u32,
        data_align: i32,
        return_address: u16,
        cie: &'a [u8],
        fde: &'a [u8],
        personality: Option<u32>,
        lsda: Option<u32>,
    },
    Windows { info: &'a [u8], handler: Option<u32>, lsda: Option<u32> },
}

impl TemplateData {
    pub fn view(&self) -> DataView<'_> {
        DataView { align: self.align, bytes: &self.bytes, relocs: self.relocs.view(), writable: self.writable }
    }
}

impl DataView<'_> {
    fn owned(self) -> TemplateData {
        TemplateData {
            align: self.align,
            // alloc: a template its caller keeps
            bytes: self.bytes.to_vec(),
            relocs: self.relocs.to_relocs(),
            writable: self.writable,
        }
    }
}

impl TemplateUnwind {
    pub fn view(&self) -> UnwindView<'_> {
        match self {
            TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda } => {
                UnwindView::Dwarf {
                    code_align: *code_align,
                    data_align: *data_align,
                    return_address: *return_address,
                    cie,
                    fde,
                    personality: *personality,
                    lsda: *lsda,
                }
            }
            TemplateUnwind::Windows { info, handler, lsda } => {
                UnwindView::Windows { info, handler: *handler, lsda: *lsda }
            }
        }
    }
}

impl UnwindView<'_> {
    fn owned(self) -> TemplateUnwind {
        match self {
            UnwindView::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda } => {
                TemplateUnwind::Dwarf {
                    code_align,
                    data_align,
                    return_address,
                    // alloc: a template its caller keeps
                    cie: cie.to_vec(),
                    fde: fde.to_vec(), // alloc: as `cie`
                    personality,
                    lsda,
                }
            }
            UnwindView::Windows { info, handler, lsda } => {
                // alloc: a template its caller keeps
                TemplateUnwind::Windows { info: info.to_vec(), handler, lsda }
            }
        }
    }
}

impl TemplateView<'_> {
    /// The function of instance `key` whose references are `refs`: [`Template::instantiate`]
    /// of the template the view is of, each part copied once.
    pub fn instantiate(self, key: InstanceKey, refs: &Refs) -> Result<CompiledFunction, TemplateError> {
        check_refs(refs)?;
        counts_its_own(self.relocs.targets(), self.profile.as_ref())?;
        if refs_len(refs) != self.refs {
            return Err(TemplateError::Refs { made: self.refs, given: refs_len(refs) });
        }
        frame::task(Tag::Codegen, |frame| {
            let data = instantiate_data(self.data, refs, self.externs, None, frame)?;
            Ok(CompiledFunction {
                key,
                arch: self.arch,
                align: self.align,
                // alloc: the instance's code, kept by the build
                code: self.code.to_vec(),
                relocs: instantiate_relocs(self.relocs, refs, self.externs, &data, None, Some(key), frame)?,
                unwind: self.unwind.map(|it| instantiate_unwind(it, refs, &data)).transpose()?,
                data,
                // alloc: kept by the build
                inlined: self.inlined.to_vec(),
                side: self.side,
                // alloc: kept by the build
                lines: self.lines.to_vec(),
                coverage: self.coverage,
                profile: self.profile,
            })
        })
    }

    /// The template the view is of, every part copied.
    pub fn owned(self) -> Template {
        Template {
            arch: self.arch,
            align: self.align,
            // alloc: a template its caller keeps, each list at its exact length
            code: self.code.to_vec(),
            relocs: self.relocs.to_relocs(),
            data: self.data.iter().map(|it| it.owned()).collect(), // alloc: as `code`
            inlined: self.inlined.to_vec(), // alloc: as `code`
            side: self.side,
            unwind: self.unwind.map(UnwindView::owned),
            lines: self.lines.to_vec(), // alloc: as `code`
            refs: self.refs,
            externs: self.externs.iter().map(|it| (*it).to_owned()).collect(), // alloc: as `code`
            coverage: self.coverage,
            profile: self.profile,
        }
    }
}

/// A static's value with what it refers to by place, as a function's [`Template`]. Never
/// merged with another by content: it is the static [`Target::Static`] names.
#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: tests vary a copy
pub struct StaticTemplate {
    pub align: u32,
    pub init: TemplateInit,
    pub data: Vec<TemplateData>,
    pub mutable: bool,
    pub thread_local: bool,
    pub section: Option<String>,
    pub imports: Vec<(String, crate::wasm::Import)>,
    pub refs: u32,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: inside `StaticTemplate`, which only tests copy
pub enum TemplateInit {
    Bytes { bytes: Vec<u8>, relocs: Relocs<Ref> },
    Zeroed { size: u64 },
}

/// Why a function and its references don't make a template, or a template and references
/// don't make a function.
#[derive(PartialEq, Eq, Debug)]
pub enum TemplateError {
    /// A relocation points at a function, static or symbol that is not among the references.
    NotReferenced(Target),
    /// A relocation points at data that is not the function's own.
    ForeignData(ContentId),
    /// A data object points at itself or at data after it.
    DataOrder { data: u32, points_at: u32 },
    /// A reference is data or a position.
    NotAReference(Target),
    /// Instantiated with another number of references than it was made against.
    Refs { made: u32, given: u32 },
    /// A place or data index past the end.
    Place(u32),
    /// An index past the end of the template's own outside symbols.
    Extern(u32),
    /// A symbol a backend says it named of its own accord is not an outside symbol's name.
    NotAnExtern(String),
    /// A relocation names a SanitizerCoverage array that is not one of a function's
    /// ([`Covered::ARRAYS`]), or a static names one.
    Coverage(u32),
    /// A relocation names counter cells ([`Target::Counter`]) that are not its function's
    /// own: another instance's (the key it names), or any from data or a static (`None`).
    Counter(Option<InstanceKey>),
    /// A relocation names PGO counters ([`Target::ProfileCounters`]) from data or a static, or
    /// from code that is not instrumented ([`Profiled::Counted`]).
    ProfileCounters,
    /// Data or a static names a function's own code ([`Ref::Own`]), which only its code can.
    Own,
    /// A function's data object (its index) is writable, which only a static's can be.
    Writable(u32),
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateError::NotReferenced(target) => {
                write!(f, "a relocation points at {target:?}, which the instance's references do not list")
            }
            TemplateError::ForeignData(id) => {
                write!(f, "a relocation points at data {id} that is not the function's own")
            }
            TemplateError::DataOrder { data, points_at } => {
                write!(f, "data object {data} points at data object {points_at}, not one before it")
            }
            TemplateError::NotAReference(target) => {
                write!(f, "{target:?} is listed as a reference, but is data, a position or a function's own")
            }
            TemplateError::Refs { made, given } => {
                write!(f, "a template made against {made} references is given {given}")
            }
            TemplateError::Place(at) => write!(f, "place {at} is past the end"),
            TemplateError::Extern(at) => write!(f, "outside symbol {at} is past the end of the template's own"),
            TemplateError::NotAnExtern(name) => {
                write!(f, "the backend named {name:?} of its own accord, which is empty or listed twice")
            }
            TemplateError::Coverage(array) => {
                write!(f, "a relocation names coverage array {array}, which is no function's own")
            }
            TemplateError::Counter(Some(key)) => {
                write!(f, "a function's code names the counter cells of instance {}, not its own", key.0)
            }
            TemplateError::Counter(None) => {
                write!(f, "data or a static names counter cells, which are only a function's code's own")
            }
            TemplateError::ProfileCounters => write!(
                f,
                "a relocation names PGO counters, which are only an instrumented function's code's own"
            ),
            TemplateError::Own => {
                write!(f, "data or a static names a function's own code, which only that code can")
            }
            TemplateError::Writable(at) => {
                write!(f, "a function's data object {at} is writable, which only a static's own data can be")
            }
        }
    }
}

pub const TEMPLATE_DOMAIN: &str = "blaze v2 template 4";

/// The outside symbols of a static's template: a static names none of its own accord.
const NO_EXTERNS: &[&str] = &[];

fn check_refs(refs: &Refs) -> Result<(), TemplateError> {
    match refs
        .iter()
        .find(|it| {
            matches!(
                it,
                Target::Data(_)
                    | Target::Position(_)
                    | Target::Coverage(_)
                    | Target::Counter(_)
                    | Target::ProfileCounters
            )
        })
    {
        // clone: an error names the target
        Some(it) => Err(TemplateError::NotAReference(it.clone())),
        None => Ok(()),
    }
}

/// The first place of `target` among `refs`.
fn place(refs: &Refs, target: &Target) -> Result<u32, TemplateError> {
    let found = refs.iter().position(|it| it == target);
    // clone: an error names the target
    found.map(|it| it as u32).ok_or_else(|| TemplateError::NotReferenced(target.clone()))
}

/// `target` by its place among `refs`, else, for an outside symbol, among `externs`.
fn reference(refs: &Refs, externs: &[String], target: &Target) -> Result<Ref, TemplateError> {
    match place(refs, target) {
        Ok(at) => Ok(Ref::Place(at)),
        Err(missing) => match target {
            Target::Extern(name) => {
                externs.iter().position(|it| it == name).map(|at| Ref::Extern(at as u32)).ok_or(missing)
            }
            _ => Err(missing),
        },
    }
}

/// The outside symbols of `extra` that `refs` does not list, each once, in `extra`'s order.
fn own_externs(refs: &Refs, extra: &[String]) -> Result<Vec<String>, TemplateError> {
    let mut externs: Vec<String> = Vec::with_capacity(extra.len());
    for name in extra {
        if name.is_empty() || externs.contains(name) {
            // clone: an error names the target
            return Err(TemplateError::NotAnExtern(name.clone()));
        }
        if !refs.iter().any(|it| matches!(it, Target::Extern(listed) if listed == name)) {
            // clone: the template keeps it, once per symbol a backend named of its own accord
            externs.push(name.clone());
        }
    }
    Ok(externs)
}

/// `relocs` with each target by its place, its index and remap in `frame`. `own` is the
/// instance whose code they are, the one whose counter cells they may name; `None` for data
/// and statics, which name none.
fn template_relocs(
    relocs: RelocsView<'_, Target>,
    refs: &Refs,
    externs: &[String],
    data: &[Data],
    before: Option<u32>,
    own: Option<InstanceKey>,
    frame: &Frame,
) -> Result<Relocs<Ref>, TemplateError> {
    relocs.try_map_in(frame, |target| {
        Ok(match target {
            Target::Data(id) => {
                let Some(at) = data.iter().position(|it| it.id == *id) else {
                    return Err(TemplateError::ForeignData(*id));
                };
                let at = at as u32;
                if let Some(data) = before
                    && at >= data
                {
                    return Err(TemplateError::DataOrder { data, points_at: at });
                }
                Ref::Data(at)
            }
            Target::Position(span) => Ref::Position(*span),
            Target::Coverage(array) if *array < Covered::ARRAYS => Ref::Coverage(*array),
            Target::Coverage(array) => return Err(TemplateError::Coverage(*array)),
            Target::Counter(key) if own == Some(*key) => Ref::Counter,
            Target::Counter(key) => return Err(TemplateError::Counter(own.map(|_| *key))),
            Target::ProfileCounters if own.is_some() => Ref::ProfileCounters,
            Target::ProfileCounters => return Err(TemplateError::ProfileCounters),
            Target::Direct(key) if own == Some(*key) => Ref::Own,
            other => reference(refs, externs, other)?,
        })
    })
}

fn template_data(
    data: &[Data],
    refs: &Refs,
    externs: &[String],
    frame: &Frame,
) -> Result<Vec<TemplateData>, TemplateError> {
    let mut made = Vec::with_capacity(data.len());
    for (at, it) in data.iter().enumerate() {
        made.push(TemplateData {
            align: it.align,
            // clone: a static's value, once a static
            bytes: it.bytes.clone(),
            relocs: template_relocs(it.relocs.view(), refs, externs, data, Some(at as u32), None, frame)?,
            writable: it.writable,
        });
    }
    Ok(made)
}

/// The data of a template with `refs` in its places, each with its content ID; a writable
/// object's is the static `owner`'s ([`Data::owned`]), which a function's data has none of.
fn instantiate_data<E: AsRef<str>>(
    data: &[DataView<'_>],
    refs: &Refs,
    externs: &[E],
    owner: Option<&ContentId>,
    frame: &Frame,
) -> Result<Vec<Data>, TemplateError> {
    let mut made: Vec<Data> = Vec::with_capacity(data.len());
    for (at, it) in data.iter().enumerate() {
        let relocs = instantiate_relocs(it.relocs, refs, externs, &made, Some(at as u32), None, frame)?;
        // alloc: the instance's data, kept by the build
        let bytes = it.bytes.to_vec();
        made.push(match (it.writable, owner) {
            (false, _) => Data::new(it.align, bytes, relocs),
            (true, Some(owner)) => Data::owned(owner, at as u32, it.align, bytes, relocs),
            (true, None) => return Err(TemplateError::Writable(at as u32)),
        });
    }
    Ok(made)
}

/// `relocs` with `refs` in its places, its index and remap in `frame`. `own` is the instance
/// whose code they are, whose counter cells [`Ref::Counter`] names; `None` for data and
/// statics.
fn instantiate_relocs<E: AsRef<str>>(
    relocs: RelocsView<'_, Ref>,
    refs: &Refs,
    externs: &[E],
    data: &[Data],
    before: Option<u32>,
    own: Option<InstanceKey>,
    frame: &Frame,
) -> Result<Relocs, TemplateError> {
    relocs.try_map_in(frame, |target| {
        Ok(match *target {
            // clone: the instance's own target, kept by the build
            Ref::Place(at) => refs.get(at as usize).cloned().ok_or(TemplateError::Place(at))?,
            Ref::Extern(at) => {
                let name = externs.get(at as usize).ok_or(TemplateError::Extern(at))?;
                // alloc: the instance's own target, kept by the build
                Target::Extern(name.as_ref().to_owned())
            }
            Ref::Data(at) => {
                if let Some(data) = before
                    && at >= data
                {
                    return Err(TemplateError::DataOrder { data, points_at: at });
                }
                Target::Data(data.get(at as usize).ok_or(TemplateError::Place(at))?.id)
            }
            Ref::Position(span) => Target::Position(span),
            Ref::Coverage(array) if array < Covered::ARRAYS => Target::Coverage(array),
            Ref::Coverage(array) => return Err(TemplateError::Coverage(array)),
            Ref::Counter => Target::Counter(own.ok_or(TemplateError::Counter(None))?),
            Ref::ProfileCounters if own.is_some() => Target::ProfileCounters,
            Ref::ProfileCounters => return Err(TemplateError::ProfileCounters),
            Ref::Own => Target::Direct(own.ok_or(TemplateError::Own)?),
        })
    })
}

/// A function's code names PGO counters only when it is instrumented: they are its record's.
fn counts_its_own(targets: &[Ref], profile: Option<&Profiled>) -> Result<(), TemplateError> {
    let counted = profile.is_some_and(|it| it.counted().is_some());
    match !counted && targets.contains(&Ref::ProfileCounters) {
        true => Err(TemplateError::ProfileCounters),
        false => Ok(()),
    }
}

fn data_index(data: &[Data], id: ContentId) -> Result<u32, TemplateError> {
    match data.iter().position(|it| it.id == id) {
        Some(at) => Ok(at as u32),
        None => Err(TemplateError::ForeignData(id)),
    }
}

fn template_unwind(unwind: Unwind, refs: &Refs, data: &[Data]) -> Result<TemplateUnwind, TemplateError> {
    let routine = |it: &Option<Target>| it.as_ref().map(|it| place(refs, it)).transpose();
    let lsda = |it: &Option<ContentId>| it.map(|it| data_index(data, it)).transpose();
    Ok(match unwind {
        Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda: area } => {
            TemplateUnwind::Dwarf {
                code_align,
                data_align,
                return_address,
                personality: routine(&personality)?,
                lsda: lsda(&area)?,
                cie,
                fde,
            }
        }
        Unwind::Windows { info, handler, lsda: area } => {
            TemplateUnwind::Windows { handler: routine(&handler)?, lsda: lsda(&area)?, info }
        }
    })
}

fn instantiate_unwind(unwind: UnwindView<'_>, refs: &Refs, data: &[Data]) -> Result<Unwind, TemplateError> {
    let routine = |it: Option<u32>| {
        // clone: the instance's own target, kept by the build
        it.map(|at| refs.get(at as usize).cloned().ok_or(TemplateError::Place(at))).transpose()
    };
    let lsda =
        |it: Option<u32>| it.map(|at| data.get(at as usize).map(|it| it.id).ok_or(TemplateError::Place(at))).transpose();
    Ok(match unwind {
        UnwindView::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda: area } => {
            Unwind::Dwarf {
                code_align,
                data_align,
                return_address,
                // alloc: kept by the build
                cie: cie.to_vec(),
                fde: fde.to_vec(), // alloc: kept by the build
                personality: routine(personality)?,
                lsda: lsda(area)?,
            }
        }
        UnwindView::Windows { info, handler, lsda: area } => {
            // alloc: kept by the build
            Unwind::Windows { info: info.to_vec(), handler: routine(handler)?, lsda: lsda(area)? }
        }
    })
}

fn refs_len(refs: &Refs) -> u32 {
    u32::try_from(refs.len()).expect("an instance refers to fewer than 2^32 things")
}

impl Template {
    /// [`Template::of_owned`] of a function the test keeps.
    #[cfg(test)]
    pub fn of(function: &CompiledFunction, refs: &Refs, extra: &[String]) -> Result<Template, TemplateError> {
        // clone: tests keep the function they make a template of; a compile's goes to `of_owned`
        Template::of_owned(function.clone(), refs, extra)
    }

    /// `function` with each of its references by its place in `refs`. `extra` are the outside
    /// symbols its backend named of its own accord ([`crate::backend::Emitted::extra`]): a
    /// relocation to one of them that `refs` lacks is kept by name in the template. The
    /// function's code, data bytes, lines, unwind information and coverage move into it.
    pub fn of_owned(function: CompiledFunction, refs: &Refs, extra: &[String]) -> Result<Template, TemplateError> {
        check_refs(refs)?;
        let externs = own_externs(refs, extra)?;
        let (relocs, mut data) = frame::task(Tag::Codegen, |frame| {
            let own = Some(function.key);
            let relocs = template_relocs(function.relocs.view(), refs, &externs, &function.data, None, own, frame)?;
            let mut data = Vec::with_capacity(function.data.len());
            for (at, it) in function.data.iter().enumerate() {
                let at = Some(at as u32);
                let relocs = template_relocs(it.relocs.view(), refs, &externs, &function.data, at, None, frame)?;
                // alloc: none: the function's bytes move in once every check has passed
                data.push(TemplateData { align: it.align, bytes: Vec::new(), relocs, writable: it.writable });
            }
            Ok::<_, TemplateError>((relocs, data))
        })?;
        counts_its_own(relocs.targets(), function.profile.as_ref())?;
        let unwind = function.unwind.map(|it| template_unwind(it, refs, &function.data)).transpose()?;
        for (made, it) in data.iter_mut().zip(function.data) {
            made.bytes = it.bytes;
        }
        Ok(Template {
            arch: function.arch,
            align: function.align,
            code: function.code,
            relocs,
            data,
            inlined: function.inlined,
            side: function.side,
            unwind,
            lines: function.lines,
            refs: refs_len(refs),
            externs,
            coverage: function.coverage,
            profile: function.profile,
        })
    }

    /// The template borrowed, its data and outside symbols listed in `frame`.
    pub fn view<'a>(&'a self, frame: &'a Frame) -> TemplateView<'a> {
        TemplateView {
            arch: self.arch,
            align: self.align,
            code: &self.code,
            relocs: self.relocs.view(),
            data: frame.alloc_slice_fill_with(self.data.len(), |at| self.data[at].view()),
            inlined: &self.inlined,
            side: self.side,
            unwind: self.unwind.as_ref().map(TemplateUnwind::view),
            lines: &self.lines,
            refs: self.refs,
            externs: frame.alloc_slice_fill_with(self.externs.len(), |at| self.externs[at].as_str()),
            // clone: the function made of the view takes it
            coverage: self.coverage.clone(),
            profile: self.profile,
        }
    }

    /// The function of instance `key` whose references are `refs`.
    pub fn instantiate(&self, key: InstanceKey, refs: &Refs) -> Result<CompiledFunction, TemplateError> {
        frame::task(Tag::Codegen, |frame| self.view(frame).instantiate(key, refs))
    }

    /// The template's content ID: two templates with one are the same function up to what
    /// fills their places.
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new(TEMPLATE_DOMAIN);
        hasher.update(&[content::arch_tag(self.arch)]);
        hasher.update(&self.align.to_le_bytes()).part(&self.code);
        content::relocs_into(&mut hasher, &self.relocs, ref_into);
        template_data_into(&mut hasher, &self.data);
        hasher.update(&(self.inlined.len() as u64).to_le_bytes());
        for code in &self.inlined {
            hasher.id(&code.0);
        }
        match &self.side {
            Some(side) => hasher.update(&[1]).id(side),
            None => hasher.update(&[0]),
        };
        let index = |hasher: &mut Hasher, it: &Option<u32>| match it {
            Some(at) => {
                hasher.update(&[1]).update(&at.to_le_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        };
        match &self.unwind {
            None => {
                hasher.update(&[0]);
            }
            Some(TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
                hasher.update(&[1]).update(&code_align.to_le_bytes()).update(&data_align.to_le_bytes());
                hasher.update(&return_address.to_le_bytes()).part(cie).part(fde);
                index(&mut hasher, personality);
                index(&mut hasher, lsda);
            }
            Some(TemplateUnwind::Windows { info, handler, lsda }) => {
                hasher.update(&[2]).part(info);
                index(&mut hasher, handler);
                index(&mut hasher, lsda);
            }
        }
        content::lines_into(&mut hasher, &self.lines);
        hasher.update(&self.refs.to_le_bytes());
        hasher.update(&(self.externs.len() as u64).to_le_bytes());
        for name in &self.externs {
            hasher.part(name.as_bytes());
        }
        content::covered_into(&mut hasher, self.coverage.as_ref());
        content::profiled_into(&mut hasher, self.profile.as_ref());
        hasher.finish()
    }
}

impl StaticTemplate {
    pub fn of(value: &Static, refs: &Refs) -> Result<StaticTemplate, TemplateError> {
        check_refs(refs)?;
        let (init, data) = frame::task(Tag::Codegen, |frame| {
            let init = match &value.init {
                Init::Bytes { bytes, relocs } => TemplateInit::Bytes {
                    // clone: a static's value, kept by the build
                    bytes: bytes.clone(),
                    relocs: template_relocs(relocs.view(), refs, &[], &value.data, None, None, frame)?,
                },
                Init::Zeroed { size } => TemplateInit::Zeroed { size: *size },
            };
            let data = template_data(&value.data, refs, &[], frame)?;
            Ok::<_, TemplateError>((init, data))
        })?;
        names_no_coverage(&init, &data)?;
        Ok(StaticTemplate {
            align: value.align,
            init,
            data,
            mutable: value.mutable,
            thread_local: value.thread_local,
            // clone: a static's value, kept by the build
            section: value.section.clone(),
            // clone: a static's value, kept by the build
            imports: value.imports.clone(),
            refs: refs_len(refs),
        })
    }

    pub fn instantiate(&self, item: ItemKey, instance: ContentId, refs: &Refs) -> Result<Static, TemplateError> {
        check_refs(refs)?;
        names_no_coverage(&self.init, &self.data)?;
        if refs_len(refs) != self.refs {
            return Err(TemplateError::Refs { made: self.refs, given: refs_len(refs) });
        }
        let (data, init) = frame::task(Tag::Codegen, |frame| {
            let views = frame.alloc_slice_fill_with(self.data.len(), |at| self.data[at].view());
            let data = instantiate_data(views, refs, NO_EXTERNS, Some(&item.0), frame)?;
            let init = match &self.init {
                TemplateInit::Bytes { bytes, relocs } => Init::Bytes {
                    // clone: a static's value, kept by the build
                    bytes: bytes.clone(),
                    relocs: instantiate_relocs(relocs.view(), refs, NO_EXTERNS, &data, None, None, frame)?,
                },
                TemplateInit::Zeroed { size } => Init::Zeroed { size: *size },
            };
            Ok::<_, TemplateError>((data, init))
        })?;
        Ok(Static {
            item,
            instance,
            align: self.align,
            init,
            data,
            mutable: self.mutable,
            thread_local: self.thread_local,
            // clone: a static's value, kept by the build
            section: self.section.clone(),
            // clone: a static's value, kept by the build
            imports: self.imports.clone(),
        })
    }
}

/// A static's value and data name no SanitizerCoverage array or PGO counters: those are a
/// function's own.
fn names_no_coverage(init: &TemplateInit, data: &[TemplateData]) -> Result<(), TemplateError> {
    let own: &[Ref] = match init {
        TemplateInit::Bytes { relocs, .. } => relocs.targets(),
        TemplateInit::Zeroed { .. } => &[],
    };
    let named = own.iter().chain(data.iter().flat_map(|it| it.relocs.targets())).find_map(|it| match *it {
        Ref::Coverage(array) => Some(TemplateError::Coverage(array)),
        Ref::ProfileCounters => Some(TemplateError::ProfileCounters),
        Ref::Own => Some(TemplateError::Own),
        _ => None,
    });
    match named {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn ref_into(hasher: &mut Hasher, target: &Ref) {
    match *target {
        Ref::Place(at) => hasher.update(&[0]).update(&at.to_le_bytes()),
        Ref::Data(at) => hasher.update(&[1]).update(&at.to_le_bytes()),
        Ref::Position(SpanRef { anchor, index }) => hasher.update(&[2]).id(&anchor).update(&index.to_le_bytes()),
        Ref::Extern(at) => hasher.update(&[3]).update(&at.to_le_bytes()),
        Ref::Coverage(array) => hasher.update(&[4]).update(&array.to_le_bytes()),
        Ref::Counter => hasher.update(&[5]),
        Ref::ProfileCounters => hasher.update(&[6]),
        Ref::Own => hasher.update(&[7]),
    };
}

fn template_data_into(hasher: &mut Hasher, data: &[TemplateData]) {
    hasher.update(&(data.len() as u64).to_le_bytes());
    for it in data {
        hasher.update(&it.align.to_le_bytes()).part(&it.bytes);
        content::relocs_into(hasher, &it.relocs, ref_into);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RelocKind, Relocation};

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn call(at: u32, to: Target) -> Relocation {
        Relocation { at, kind: RelocKind::PcRel32, target: to, addend: -4 }
    }

    /// `Vec<A>::reserve` calling `Vec<A>::try_grow_to` and reading a constant that points at
    /// a static.
    fn reserve(own: InstanceKey, grow: InstanceKey, item: ItemKey) -> (CompiledFunction, Vec<Target>) {
        let refs = vec![
            Target::Function(grow),
            Target::Static { item, instance: ContentId([9; 32]) },
            Target::Extern("memcpy".to_owned()),
        ];
        let inner = Data::new(8, vec![1, 2, 3, 4, 5, 6, 7, 8], Relocs::new());
        let outer = Data::new(
            8,
            vec![0; 16],
            vec![
                Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Data(inner.id), addend: 0 },
                Relocation { at: 8, kind: RelocKind::Abs64, target: refs[0].clone(), addend: 0 },
            ]
            .into(),
        );
        let function = CompiledFunction {
            key: own,
            arch: Arch::X86_64,
            align: 16,
            code: vec![0xe8, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0, 0xc3],
            relocs: vec![
                call(1, refs[0].clone()),
                call(6, refs[2].clone()),
                Relocation { at: 6, kind: RelocKind::Abs64, target: Target::Data(outer.id), addend: 0 },
                Relocation { at: 1, kind: RelocKind::Abs64, target: refs[1].clone(), addend: 8 },
            ]
            .into(),
            data: vec![inner, outer],
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: vec![Line { at: 0, span: SpanRef { anchor: ContentId([5; 32]), index: 2 } }],
            coverage: None,
            profile: None,
        };
        (function, refs)
    }

    /// The heap blocks of a relocation table: its rows, targets and wide addends, each one
    /// when not empty.
    fn table_blocks<T>(relocs: &Relocs<T>) -> u64 {
        [relocs.rows().len(), relocs.targets().len(), relocs.wide().len()].into_iter().map(|it| u64::from(it != 0)).sum()
    }

    /// The heap blocks a function owns: what making it must allocate, and no more.
    fn owned_blocks(function: &CompiledFunction) -> u64 {
        let list = |len: usize| u64::from(len != 0);
        let named = |it: &Target| u64::from(matches!(it, Target::Extern(name) | Target::Weak(name) if !name.is_empty()));
        let table = |relocs: &Relocs| table_blocks(relocs) + relocs.targets().iter().map(named).sum::<u64>();
        let routine = |it: &Option<Target>| it.as_ref().map_or(0, named);
        let unwind = match &function.unwind {
            None => 0,
            Some(Unwind::Dwarf { cie, fde, personality, .. }) => list(cie.len()) + list(fde.len()) + routine(personality),
            Some(Unwind::Windows { info, handler, .. }) => list(info.len()) + routine(handler),
        };
        let data: u64 = function.data.iter().map(|it| list(it.bytes.len()) + table(&it.relocs)).sum();
        list(function.code.len())
            + table(&function.relocs)
            + list(function.data.len())
            + data
            + list(function.inlined.len())
            + list(function.lines.len())
            + unwind
            + function.coverage.as_ref().map_or(0, |it| list(it.pcs.len()))
    }

    /// A warm instantiate allocates exactly the blocks the function owns: each part copied
    /// once, the template's views and the tables' indexes in frames.
    #[test]
    fn a_warm_instantiate_allocates_only_what_the_function_keeps() {
        let (function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        // The code, the relocations' rows, targets and `memcpy`'s name, the data list, `inner`'s
        // bytes, `outer`'s bytes, rows and targets, and the lines.
        assert_eq!(owned_blocks(&function), 10);
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);
        let made = blaze_alloc_count::assert_at_most(owned_blocks(&function), "a warm instantiate", || {
            template.instantiate(function.key, &refs)
        })
        .unwrap();
        assert_eq!(made, function);
    }

    /// The template of a compile takes the function's parts over: it allocates only its
    /// relocation tables and its list of data.
    #[test]
    fn a_compiles_template_takes_its_parts_over() {
        let (function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let expected = Template::of(&function, &refs, &[]).unwrap();
        assert_eq!(Template::of_owned(function.clone(), &refs, &[]).unwrap(), expected);
        let lists = table_blocks(&function.relocs)
            + u64::from(!function.data.is_empty())
            + function.data.iter().map(|it| table_blocks(&it.relocs)).sum::<u64>();
        // The function's rows and targets, the data list, and `outer`'s rows and targets.
        assert_eq!(lists, 5);
        let given = function.clone();
        let made = blaze_alloc_count::assert_at_most(lists, "a compile's template: its tables", || {
            Template::of_owned(given, &refs, &[])
        })
        .unwrap();
        assert_eq!(made, expected);
        assert_eq!(made.instantiate(function.key, &refs).unwrap(), function);
    }

    /// A view's owned template is the template, and a static's template, made and filled
    /// through frames, gives back its static.
    #[test]
    fn a_view_owns_back_its_template() {
        let (mut function, mut refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        refs.push(Target::Extern("rust_eh_personality".to_owned()));
        function.unwind = Some(Unwind::Dwarf {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            cie: vec![0x0c, 7, 8],
            fde: vec![0x0e, 16],
            personality: Some(refs[3].clone()),
            lsda: Some(function.data[0].id),
        });
        function.relocs.push(call(1, Target::Extern("bcmp".to_owned())));
        let template = Template::of(&function, &refs, &["bcmp".to_owned()]).unwrap();
        let owned = frame::task(Tag::Codegen, |frame| template.view(frame).owned());
        assert_eq!(owned, template);
        assert_eq!(owned.id(), template.id());
        let made = frame::task(Tag::Codegen, |frame| template.view(frame).instantiate(function.key, &refs)).unwrap();
        assert_eq!(made, function);
    }

    #[test]
    fn unwinding_names_its_personality_by_place_and_its_lsda_by_index() {
        let (mut function, mut refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        refs.push(Target::Extern("rust_eh_personality".to_owned()));
        let lsda = function.data[0].id;
        function.unwind = Some(Unwind::Dwarf {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            cie: vec![0x0c, 7, 8],
            fde: vec![0x0e, 16],
            personality: Some(refs[3].clone()),
            lsda: Some(lsda),
        });
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert!(matches!(
            template.unwind,
            Some(TemplateUnwind::Dwarf { personality: Some(3), lsda: Some(0), .. })
        ));
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);

        let mut foreign = function.clone();
        foreign.unwind = Some(Unwind::Windows { info: vec![1], handler: None, lsda: Some(ContentId([8; 32])) });
        assert_eq!(Template::of(&foreign, &refs, &[]).unwrap_err(), TemplateError::ForeignData(ContentId([8; 32])));
        let mut unlisted = function;
        unlisted.unwind = Some(Unwind::Windows {
            info: vec![1],
            handler: Some(Target::Extern("__CxxFrameHandler3".to_owned())),
            lsda: None,
        });
        assert!(matches!(Template::of(&unlisted, &refs, &[]), Err(TemplateError::NotReferenced(_))));
    }

    #[test]
    fn coverage_is_the_functions_own() {
        let (mut function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        function.relocs.push(Relocation { at: 1, kind: RelocKind::PcRel32, target: Target::Coverage(0), addend: 1 });
        function.coverage = Some(Covered { counters: 2, flags: 0, guards: 0, pcs: vec![(0, 1), (5, 0)] });
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert!(template.relocs.targets().contains(&Ref::Coverage(0)));
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);
        assert_ne!(template.id(), Template { coverage: None, ..template.clone() }.id());

        let mut past = function.clone();
        past.relocs = past.relocs.map(|it| match it {
            Target::Coverage(_) => Target::Coverage(Covered::ARRAYS),
            other => other.clone(),
        });
        assert_eq!(Template::of(&past, &refs, &[]).unwrap_err(), TemplateError::Coverage(Covered::ARRAYS));
        let mut listed = refs.clone();
        listed.push(Target::Coverage(0));
        assert_eq!(
            Template::of(&function, &listed, &[]).unwrap_err(),
            TemplateError::NotAReference(Target::Coverage(0))
        );
        let value = Static {
            item: ItemKey(ContentId([3; 32])),
            instance: ContentId([4; 32]),
            align: 8,
            init: Init::Bytes {
                bytes: vec![0; 8],
                relocs: vec![Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Coverage(1), addend: 0 }].into(),
            },
            data: Vec::new(),
            mutable: false,
            thread_local: false,
            section: None,
            imports: Vec::new(),
        };
        assert_eq!(StaticTemplate::of(&value, &refs).unwrap_err(), TemplateError::Coverage(1));
    }

    /// An instrumented function's counters and record are its own: kept by the template, in
    /// its ID, given back whatever fills its places, and named by nothing else.
    #[test]
    fn pgo_counters_are_the_instrumented_functions_own() {
        let (mut function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        function.relocs.push(Relocation { at: 1, kind: RelocKind::PcRel32, target: Target::ProfileCounters, addend: 4 });
        let record = crate::ProfileRecord { name: [6; 16], name_ref: 1, hash: 2, counters: 3, version: 10 };
        let uncounted = function.clone();
        function.profile = Some(Profiled::Counted(record));
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert!(template.relocs.targets().contains(&Ref::ProfileCounters));
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);
        let (other, other_refs) = reserve(key(4), key(5), ItemKey(ContentId([3; 32])));
        let other = CompiledFunction { relocs: function.relocs.clone(), profile: function.profile, ..other };
        assert_eq!(template.instantiate(other.key, &other_refs).unwrap().profile, function.profile);
        assert_ne!(template.id(), Template { profile: None, ..template.clone() }.id());
        let used = Template { profile: Some(Profiled::Used { mismatched: true }), ..template.clone() };
        assert_ne!(used.id(), Template { profile: Some(Profiled::Used { mismatched: false }), ..used.clone() }.id());

        assert_eq!(Template::of(&uncounted, &refs, &[]).unwrap_err(), TemplateError::ProfileCounters);
        assert_eq!(used.instantiate(function.key, &refs).unwrap_err(), TemplateError::ProfileCounters);
        let mut from_data = function.clone();
        from_data.data[0].relocs.push(Relocation { at: 0, kind: RelocKind::Abs64, target: Target::ProfileCounters, addend: 0 });
        assert_eq!(Template::of(&from_data, &refs, &[]).unwrap_err(), TemplateError::ProfileCounters);
        let mut listed = refs.clone();
        listed.push(Target::ProfileCounters);
        assert_eq!(
            Template::of(&function, &listed, &[]).unwrap_err(),
            TemplateError::NotAReference(Target::ProfileCounters)
        );
        let value = Static {
            item: ItemKey(ContentId([3; 32])),
            instance: ContentId([4; 32]),
            align: 8,
            init: Init::Bytes {
                bytes: vec![0; 8],
                relocs: vec![Relocation { at: 0, kind: RelocKind::Abs64, target: Target::ProfileCounters, addend: 0 }].into(),
            },
            data: Vec::new(),
            mutable: false,
            thread_local: false,
            section: None,
            imports: Vec::new(),
        };
        assert_eq!(StaticTemplate::of(&value, &refs).unwrap_err(), TemplateError::ProfileCounters);
    }

    /// A function's reference into its own code needs no place: siblings share one template,
    /// and each instance made of it points into its own code.
    #[test]
    fn a_reference_into_its_own_code_is_the_instances_own() {
        let into_itself = |own: InstanceKey, grow: InstanceKey| {
            let (mut function, refs) = reserve(own, grow, ItemKey(ContentId([3; 32])));
            function.relocs.push(Relocation { at: 1, kind: RelocKind::PcRel32, target: Target::Direct(own), addend: 6 });
            (function, refs)
        };
        let (a, a_refs) = into_itself(key(1), key(2));
        let (b, b_refs) = into_itself(key(4), key(5));
        let (ta, tb) = (Template::of(&a, &a_refs, &[]).unwrap(), Template::of(&b, &b_refs, &[]).unwrap());
        assert_eq!(ta, tb);
        assert!(ta.relocs.targets().contains(&Ref::Own));
        assert_eq!(ta.instantiate(a.key, &a_refs).unwrap(), a);
        assert_eq!(ta.instantiate(b.key, &b_refs).unwrap(), b);
        let made = frame::task(Tag::Codegen, |frame| ta.view(frame).instantiate(b.key, &b_refs)).unwrap();
        assert_eq!(made, b);

        let (mut from_data, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        from_data.data[0].relocs.push(Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Direct(key(1)), addend: 0 });
        assert_eq!(
            Template::of(&from_data, &refs, &[]).unwrap_err(),
            TemplateError::NotReferenced(Target::Direct(key(1)))
        );
        let named = StaticTemplate {
            align: 8,
            init: TemplateInit::Bytes {
                bytes: vec![0; 8],
                relocs: vec![Relocation { at: 0, kind: RelocKind::Abs64, target: Ref::Own, addend: 0 }].into(),
            },
            data: Vec::new(),
            mutable: false,
            thread_local: false,
            section: None,
            imports: Vec::new(),
            refs: 0,
        };
        assert_eq!(named.instantiate(ItemKey(ContentId([3; 32])), ContentId([4; 32]), &[]).unwrap_err(), TemplateError::Own);
    }

    /// Two counting siblings share one template, and each instance made of it counts in its
    /// own cells.
    #[test]
    fn a_counter_is_the_instances_own() {
        let counting = |own: InstanceKey, grow: InstanceKey| {
            let (mut function, refs) = reserve(own, grow, ItemKey(ContentId([3; 32])));
            for at in [0, 8] {
                function.relocs.push(Relocation { at: 1, kind: RelocKind::PcRel32, target: Target::Counter(own), addend: at });
            }
            (function, refs)
        };
        let (a, a_refs) = counting(key(1), key(2));
        let (b, b_refs) = counting(key(4), key(5));
        let (ta, tb) = (Template::of(&a, &a_refs, &[]).unwrap(), Template::of(&b, &b_refs, &[]).unwrap());
        assert_eq!(ta, tb);
        assert!(ta.relocs.targets().contains(&Ref::Counter));
        assert_eq!(ta.instantiate(a.key, &a_refs).unwrap(), a);
        assert_eq!(ta.instantiate(b.key, &b_refs).unwrap(), b);
        let (plain, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        assert_ne!(ta.id(), Template::of(&plain, &refs, &[]).unwrap().id());

        let mut other = plain.clone();
        other.relocs.push(Relocation { at: 1, kind: RelocKind::PcRel32, target: Target::Counter(key(9)), addend: 0 });
        assert_eq!(Template::of(&other, &refs, &[]).unwrap_err(), TemplateError::Counter(Some(key(9))));
        let mut from_data = plain.clone();
        from_data.data[0].relocs.push(Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Counter(plain.key), addend: 0 });
        assert_eq!(Template::of(&from_data, &refs, &[]).unwrap_err(), TemplateError::Counter(None));
        let mut listed = refs.clone();
        listed.push(Target::Counter(plain.key));
        assert_eq!(
            Template::of(&plain, &listed, &[]).unwrap_err(),
            TemplateError::NotAReference(Target::Counter(plain.key))
        );
        let value = Static {
            item: ItemKey(ContentId([3; 32])),
            instance: ContentId([4; 32]),
            align: 8,
            init: Init::Bytes {
                bytes: vec![0; 8],
                relocs: vec![Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Counter(key(1)), addend: 0 }].into(),
            },
            data: Vec::new(),
            mutable: false,
            thread_local: false,
            section: None,
            imports: Vec::new(),
        };
        assert_eq!(StaticTemplate::of(&value, &refs).unwrap_err(), TemplateError::Counter(None));
    }

    #[test]
    fn a_template_gives_back_its_function() {
        let (function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);
    }

    #[test]
    fn a_symbol_the_backend_named_is_kept_by_name() {
        let (mut function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let bcmp = Target::Extern("bcmp".to_owned());
        function.relocs.push(call(1, bcmp.clone()));
        assert_eq!(Template::of(&function, &refs, &[]).unwrap_err(), TemplateError::NotReferenced(bcmp));
        // `memcpy` is listed already, so it stays a place.
        let extra = ["memcpy".to_owned(), "bcmp".to_owned()];
        let template = Template::of(&function, &refs, &extra).unwrap();
        assert_eq!(template.externs, ["bcmp".to_owned()]);
        assert!(template.relocs.targets().contains(&Ref::Extern(0)));
        assert_eq!(template.instantiate(function.key, &refs).unwrap(), function);
        assert_ne!(template.id(), Template { externs: vec!["memcmp".to_owned()], ..template.clone() }.id());
        let twice = ["bcmp".to_owned(), "bcmp".to_owned()];
        assert_eq!(
            Template::of(&function, &refs, &twice).unwrap_err(),
            TemplateError::NotAnExtern("bcmp".to_owned())
        );
    }

    #[test]
    fn siblings_up_to_callee_renaming_share_one_template() {
        let (a, a_refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let (b, b_refs) = reserve(key(4), key(5), ItemKey(ContentId([6; 32])));
        let (ta, tb) = (Template::of(&a, &a_refs, &[]).unwrap(), Template::of(&b, &b_refs, &[]).unwrap());
        assert_eq!(ta, tb);
        assert_eq!(ta.id(), tb.id());
        // `b`'s data points at its callee, so its ID is made again from the new place.
        assert_eq!(ta.instantiate(b.key, &b_refs).unwrap(), b);
    }

    #[test]
    fn a_constant_is_not_a_place() {
        let (a, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        let mut b = a.clone();
        b.code[10] = 0xcc;
        assert_ne!(Template::of(&a, &refs, &[]).unwrap().id(), Template::of(&b, &refs, &[]).unwrap().id());
    }

    #[test]
    fn what_is_not_referenced_is_refused() {
        let (function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        // Relocations are in offset order: the static's at 1 comes before `memcpy`'s at 6.
        assert_eq!(
            Template::of(&function, &refs[..1], &[]).unwrap_err(),
            TemplateError::NotReferenced(refs[1].clone())
        );
        assert_eq!(
            Template::of(&function, &refs[..2], &[]).unwrap_err(),
            TemplateError::NotReferenced(refs[2].clone())
        );
        let template = Template::of(&function, &refs, &[]).unwrap();
        assert_eq!(
            template.instantiate(function.key, &refs[..2]).unwrap_err(),
            TemplateError::Refs { made: 3, given: 2 }
        );
    }

    #[test]
    fn data_out_of_order_is_refused() {
        let (mut function, refs) = reserve(key(1), key(2), ItemKey(ContentId([3; 32])));
        function.data.reverse();
        assert!(matches!(
            Template::of(&function, &refs, &[]),
            Err(TemplateError::DataOrder { data: 0, points_at: 1 })
        ));
    }

    #[test]
    fn a_static_keeps_what_it_is() {
        let item = ItemKey(ContentId([3; 32]));
        let refs = vec![Target::Function(key(2))];
        let text = Data::new(1, b"hi".to_vec(), Relocs::new());
        let value = Static {
            item,
            instance: ContentId([4; 32]),
            align: 8,
            init: Init::Bytes {
                bytes: vec![0; 24],
                relocs: vec![
                    Relocation { at: 0, kind: RelocKind::Abs64, target: refs[0].clone(), addend: 0 },
                    Relocation { at: 8, kind: RelocKind::Abs64, target: Target::Data(text.id), addend: 0 },
                ]
                .into(),
            },
            data: vec![text],
            mutable: true,
            thread_local: false,
            section: Some(".init_array".to_owned()),
            imports: vec![(
                "host_log".to_owned(),
                crate::wasm::Import {
                    module: "env".to_owned(),
                    field: "log".to_owned(),
                    ty: crate::wasm::Signature { params: vec![crate::wasm::ValType::I32], results: Vec::new() },
                },
            )],
        };
        let template = StaticTemplate::of(&value, &refs).unwrap();
        assert_eq!(template.instantiate(item, value.instance, &refs).unwrap(), value);

        let zeroed = Static { init: Init::Zeroed { size: 1 << 20 }, data: Vec::new(), ..value };
        let template = StaticTemplate::of(&zeroed, &refs).unwrap();
        assert_eq!(template.instantiate(item, zeroed.instance, &refs).unwrap(), zeroed);
    }

    /// A static's writable object is its own after a template, its ID its static's; another
    /// static of the same template has another, and a function's data cannot be writable.
    #[test]
    fn a_static_keeps_its_writable_data_its_own() {
        let item = ItemKey(ContentId([3; 32]));
        let nested = Data::owned(&item.0, 0, 4, vec![7; 4], Relocs::new());
        let value = Static {
            item,
            instance: ContentId([4; 32]),
            align: 8,
            init: Init::Bytes {
                bytes: vec![0; 8],
                relocs: vec![Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Data(nested.id), addend: 0 }]
                    .into(),
            },
            data: vec![nested],
            mutable: true,
            thread_local: false,
            section: None,
            imports: Vec::new(),
        };
        let template = StaticTemplate::of(&value, &[]).unwrap();
        assert!(template.data[0].writable);
        assert_eq!(template.instantiate(item, value.instance, &[]).unwrap(), value);
        let other = ItemKey(ContentId([5; 32]));
        let elsewhere = template.instantiate(other, value.instance, &[]).unwrap();
        assert_ne!(elsewhere.data[0].id, value.data[0].id);
        assert_eq!(elsewhere.init, Init::Bytes {
            bytes: vec![0; 8],
            relocs: vec![Relocation {
                at: 0,
                kind: RelocKind::Abs64,
                target: Target::Data(elsewhere.data[0].id),
                addend: 0,
            }]
            .into(),
        });

        let (function, refs) = reserve(key(1), key(2), item);
        let mut function = Template::of(&function, &refs, &[]).unwrap();
        function.data[0].writable = true;
        assert_eq!(function.instantiate(key(1), &refs).unwrap_err(), TemplateError::Writable(0));
    }
}
