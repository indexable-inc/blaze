//! The symbol table.

use crate::expr::ExprRef;
use crate::intern::{Interner, Name};
use crate::section::SectionId;
use crate::source::Span;
use std::collections::HashMap;

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SymbolId(pub u32);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Binding {
    Local,
    Global,
    Weak,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum SymType {
    #[default]
    NoType,
    Object,
    Func,
    Section,
    File,
    Tls,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum Visibility {
    #[default]
    Default,
    Internal,
    Hidden,
    Protected,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SymbolValue {
    /// Referenced but never defined: the linker must supply it.
    Undefined,
    /// A label: the address of fragment `frag` in `section`.
    Label { section: SectionId, frag: u32 },
    /// Defined by `.set` / `.equ` / `=`.
    Expr(ExprRef),
    /// A `.comm` / `.lcomm` tentative definition.
    Common { size: u64, align: u64 },
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Symbol {
    pub name: Name,
    pub value: SymbolValue,
    pub binding: Binding,
    pub ty: SymType,
    pub visibility: Visibility,
    pub size: Option<ExprRef>,
    /// The symbol this one takes its size from, for a `.set` alias made
    /// before the symbol it names was defined; see
    /// [`Assembler::symbol_size`](crate::assembler::Assembler).
    pub(crate) size_from: Option<SymbolId>,
    /// Where the symbol was defined; dummy while only referenced.
    pub def_span: Span,
    /// First place the symbol was mentioned, for "undefined symbol" errors.
    pub first_use: Span,
    /// Set for the synthetic symbols behind numeric local labels (`1:`).
    pub local_number: Option<u32>,
    /// `.set` symbols may be redefined; plain labels may not.
    pub redefinable: bool,
    pub used: bool,
    /// When the symbol was last defined, counting definitions in the order
    /// they were read; 0 while it is undefined. See
    /// [`SymbolTable::mark_defined`].
    pub def_order: u32,
    /// Bits the backend recorded on the label as it was defined; see
    /// [`crate::arch::Architecture::label_flags`].
    pub target_flags: u8,
    /// Named by a directive that describes the symbol's entry — `.globl`,
    /// `.local`, `.type`, `.size` or a visibility — which is enough for GNU
    /// as to write an undefined one; see `collect_symbols` in the ELF writer.
    pub(crate) declared: bool,
    /// Named by `.local` before anything defined it, which makes a later
    /// `.comm` reserve the block in `.bss` rather than leave it common.
    pub(crate) declared_local: bool,
}

impl Symbol {
    pub fn is_defined(&self) -> bool {
        !matches!(self.value, SymbolValue::Undefined)
    }
}

#[derive(Default)]
#[non_exhaustive]
pub struct SymbolTable {
    syms: Vec<Symbol>,
    by_name: HashMap<Name, SymbolId>,
    /// For each numeric local label `N`, the synthetic symbols created for it,
    /// in definition order.
    locals: HashMap<u32, LocalSlots>,
    /// Definitions read so far.
    definitions: u32,
}

#[derive(Default)]
struct LocalSlots {
    /// One entry per `N:` occurrence, created on demand by forward references.
    slots: Vec<SymbolId>,
    /// How many of `slots` have actually been defined so far.
    defined: usize,
}

impl SymbolTable {
    /// Not API.
    #[doc(hidden)]
    pub fn new() -> SymbolTable {
        SymbolTable::default()
    }

    pub fn len(&self) -> usize {
        self.syms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.syms.is_empty()
    }

    pub fn get(&self, id: SymbolId) -> &Symbol {
        &self.syms[id.0 as usize]
    }

    /// Not API.
    #[doc(hidden)]
    pub fn get_mut(&mut self, id: SymbolId) -> &mut Symbol {
        &mut self.syms[id.0 as usize]
    }

    pub fn iter(&self) -> impl Iterator<Item = (SymbolId, &Symbol)> {
        self.syms
            .iter()
            .enumerate()
            .map(|(i, s)| (SymbolId(i as u32), s))
    }

    pub fn lookup(&self, name: Name) -> Option<SymbolId> {
        self.by_name.get(&name).copied()
    }

    /// Finds `name`, creating an undefined entry if it is new.
    /// Not API.
    #[doc(hidden)]
    pub fn intern(&mut self, name: Name, span: Span) -> SymbolId {
        if let Some(&id) = self.by_name.get(&name) {
            return id;
        }
        let id = self.push(Symbol {
            name,
            value: SymbolValue::Undefined,
            binding: Binding::Local,
            ty: SymType::NoType,
            visibility: Visibility::Default,
            size: None,
            size_from: None,
            def_span: Span::DUMMY,
            first_use: span,
            local_number: None,
            redefinable: false,
            used: false,
            def_order: 0,
            target_flags: 0,
            declared: false,
            declared_local: false,
        });
        self.by_name.insert(name, id);
        id
    }

    /// Creates the symbol that stands for a whole section.
    ///
    /// It is deliberately not registered by name: `.text` as a section symbol
    /// and `.text` as a user-written label are different things.
    /// Not API.
    #[doc(hidden)]
    pub fn intern_section(&mut self, name: Name, section: SectionId) -> SymbolId {
        self.push(Symbol {
            name,
            value: SymbolValue::Label { section, frag: 0 },
            binding: Binding::Local,
            ty: SymType::Section,
            visibility: Visibility::Default,
            size: None,
            size_from: None,
            def_span: Span::DUMMY,
            first_use: Span::DUMMY,
            local_number: None,
            redefinable: false,
            used: true,
            def_order: 0,
            target_flags: 0,
            declared: false,
            declared_local: false,
        })
    }

    /// Records that `id` has just been given a definition.
    ///
    /// Which of two symbols was defined first is not otherwise recoverable once
    /// the file is read, and it matters where the reference assembler decides
    /// something as it reads: GNU as folds `size = end - start` to a constant
    /// only if both labels were already defined at that line.
    /// Not API.
    #[doc(hidden)]
    pub fn mark_defined(&mut self, id: SymbolId) {
        self.definitions += 1;
        self.syms[id.0 as usize].def_order = self.definitions;
    }

    fn push(&mut self, s: Symbol) -> SymbolId {
        let id = SymbolId(self.syms.len() as u32);
        self.syms.push(s);
        id
    }

    /// Resolves a backward reference `Nb` to the most recent `N:`.
    /// Not API.
    #[doc(hidden)]
    pub fn local_backward(&self, n: u32, _span: Span) -> Option<SymbolId> {
        let slots = self.locals.get(&n)?;
        if slots.defined == 0 {
            return None;
        }
        Some(slots.slots[slots.defined - 1])
    }

    /// Resolves a forward reference `Nf` to the *next* `N:` to be defined,
    /// creating a placeholder symbol for it if that definition has not been
    /// seen yet.
    /// Not API.
    #[doc(hidden)]
    pub fn local_forward(&mut self, n: u32, span: Span, interner: &mut Interner) -> SymbolId {
        let idx = self.locals.entry(n).or_default().defined;
        self.local_slot(n, idx, span, interner)
    }

    /// Claims the slot for the next `N:` definition.
    /// Not API.
    #[doc(hidden)]
    pub fn local_define_slot(&mut self, n: u32, span: Span, interner: &mut Interner) -> SymbolId {
        let idx = self.locals.entry(n).or_default().defined;
        let id = self.local_slot(n, idx, span, interner);
        self.locals.get_mut(&n).expect("slot just created").defined = idx + 1;
        id
    }

    fn local_slot(&mut self, n: u32, idx: usize, span: Span, interner: &mut Interner) -> SymbolId {
        if let Some(&id) = self.locals.get(&n).and_then(|s| s.slots.get(idx)) {
            return id;
        }
        // The NUL byte makes these names unspellable in source, so a synthetic
        // local can never collide with a user symbol.
        let name = interner.intern(&format!(".L\u{0}{n}.{idx}"));
        let id = self.push(Symbol {
            name,
            value: SymbolValue::Undefined,
            binding: Binding::Local,
            ty: SymType::NoType,
            visibility: Visibility::Default,
            size: None,
            size_from: None,
            def_span: Span::DUMMY,
            first_use: span,
            local_number: Some(n),
            redefinable: false,
            used: false,
            def_order: 0,
            target_flags: 0,
            declared: false,
            declared_local: false,
        });
        let slots = self.locals.entry(n).or_default();
        debug_assert_eq!(
            slots.slots.len(),
            idx,
            "local label slots must be filled in order"
        );
        slots.slots.push(id);
        id
    }

    /// Numeric local labels that were referenced forward but never defined.
    /// Not API.
    #[doc(hidden)]
    pub fn undefined_locals(&self) -> impl Iterator<Item = (SymbolId, u32)> + '_ {
        self.locals
            .iter()
            .flat_map(|(&n, s)| s.slots[s.defined..].iter().map(move |&id| (id, n)))
    }
}
