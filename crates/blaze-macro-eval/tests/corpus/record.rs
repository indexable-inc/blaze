//! Each call of a proc macro as the front end made it, with what blaze-macro-eval's expander
//! gave (the output the front end goes on with).

use std::{
    any::Any,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use base_db::{ContentId, Crate, Db, Env};
use hir_expand::proc_macro::{
    Expanded, ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind,
};
use blaze_core::lock::Lock;
use span::Span;
use tt::TopSubtree;

pub(crate) enum Outcome {
    Expanded(TopSubtree),
    Panicked(String),
    Failed(String),
}

impl Outcome {
    fn of(result: &Result<Expanded, ProcMacroExpansionError>) -> Outcome {
        match result {
            Ok(expanded) => Outcome::Expanded(expanded.tree.copy()),
            Err(ProcMacroExpansionError::Panic(message)) => Outcome::Panicked(message.clone()),
            Err(ProcMacroExpansionError::System(why)) => Outcome::Failed(why.clone()),
        }
    }

    pub(crate) fn expanded(&self) -> Option<&TopSubtree> {
        match self {
            Outcome::Expanded(tree) => Some(tree),
            _ => None,
        }
    }
}

/// One call, owned, as the front end gave it.
pub(crate) struct Call {
    pub(crate) krate: Box<str>,
    pub(crate) name: Box<str>,
    pub(crate) kind: ProcMacroKind,
    pub(crate) calling: Option<Crate>,
    pub(crate) subtree: TopSubtree,
    pub(crate) attrs: Option<TopSubtree>,
    pub(crate) env: Env,
    pub(crate) def_site: Span,
    pub(crate) call_site: Span,
    pub(crate) mixed_site: Span,
    pub(crate) current_dir: String,
}

impl Call {
    pub(crate) fn call(&self) -> ProcMacroCall<'_> {
        ProcMacroCall {
            subtree: &self.subtree,
            attrs: self.attrs.as_ref(),
            env: &self.env,
            def_site: self.def_site,
            call_site: self.call_site,
            mixed_site: self.mixed_site,
            current_dir: &self.current_dir,
        }
    }

    /// What the call is about: the name of the item a derive or an attribute is on, the first
    /// identifier a function-like macro is given.
    pub(crate) fn subject(&self) -> Option<String> {
        let mut idents =
            self.subtree.as_token_trees().iter_flat_tokens().filter_map(|it| match it {
                tt::TokenTree::Leaf(tt::Leaf::Ident(ident)) => Some(ident.sym.as_str().to_owned()),
                _ => None,
            });
        if self.kind == ProcMacroKind::Bang {
            return idents.next();
        }
        let item =
            |it: &str| matches!(it, "struct" | "enum" | "union" | "fn" | "trait" | "type" | "mod");
        idents.by_ref().find(|it| item(it))?;
        idents.next()
    }
}

/// Whether `tree` has the identifier `name`.
pub(crate) fn has_ident(tree: &TopSubtree, name: &str) -> bool {
    tree.as_token_trees()
        .iter_flat_tokens()
        .any(|it| matches!(it, tt::TokenTree::Leaf(tt::Leaf::Ident(ident)) if ident.sym.as_str() == name))
}

/// The text of each literal of `tree`, as written.
pub(crate) fn literals(tree: &TopSubtree) -> Vec<String> {
    tree.as_token_trees()
        .iter_flat_tokens()
        .filter_map(|it| match it {
            // As written: a string's quotes and a suffix are part of it (`"corpus-calls"`, `1u8`).
            tt::TokenTree::Leaf(tt::Leaf::Literal(literal)) => Some(literal.to_string()),
            _ => None,
        })
        .collect()
}

pub(crate) struct Record {
    pub(crate) call: Call,
    pub(crate) eval: Outcome,
    /// How long blaze-macro-eval took to give it, the wall clock's.
    pub(crate) took: Duration,
}

impl Record {
    /// The macro and its input, as a failure names the call.
    pub(crate) fn what(&self) -> String {
        let input = self.call.subtree.to_string();
        let input: String = match input.char_indices().nth(100) {
            Some((at, _)) => format!("{}..", &input[..at]),
            None => input,
        };
        format!("`{}::{}` on `{input}`", self.call.krate, self.call.name)
    }
}

pub(crate) type Records = Arc<Lock<Vec<Record>>>;

/// No records yet: what a recorder records into.
pub(crate) fn records() -> Records {
    Arc::new(Lock::new("macro.test.corpus.records", Vec::new()))
}

/// Whether a recorder passes the front end's runs ahead of a call's turn on to its expander
/// (`ProcMacroExpander::speculate`), as a build does, counting them; or runs each ordered call
/// at its turn alone.
#[derive(Clone)]
pub(crate) enum Ahead {
    On(Arc<AtomicUsize>),
    Off,
}

/// blaze-macro-eval's expander of one macro, recording each call with what it gave.
pub(crate) struct Recorder {
    krate: Box<str>,
    name: Box<str>,
    kind: ProcMacroKind,
    eval: Arc<dyn ProcMacroExpander>,
    records: Records,
    ahead: Ahead,
}

/// A call that panicked records nothing: the records kept are each whole.
impl std::panic::RefUnwindSafe for Recorder {}

impl Recorder {
    /// `eval`, of crate `krate`, recording into `records`, its calls run ahead as a build runs
    /// them.
    pub(crate) fn wrap(krate: &str, eval: ProcMacro, records: &Records) -> ProcMacro {
        Recorder::wrap_ahead(krate, eval, records, Ahead::On(Arc::default()))
    }

    /// [`Recorder::wrap`], its calls run ahead or not as `ahead` says.
    pub(crate) fn wrap_ahead(
        krate: &str,
        eval: ProcMacro,
        records: &Records,
        ahead: Ahead,
    ) -> ProcMacro {
        let ProcMacro { name, kind, expander, disabled } = eval;
        let recorder = Recorder {
            krate: krate.into(),
            name: name.as_str().into(),
            kind,
            eval: expander,
            records: records.clone(),
            ahead,
        };
        ProcMacro { name, kind, expander: Arc::new(recorder), disabled }
    }

    fn run(
        &self,
        db: &Db,
        calling: Option<Crate>,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        let start = Instant::now();
        let expanded = match calling {
            Some(krate) => self.eval.expand_in(db, krate, call),
            None => self.eval.expand(db, call),
        };
        let took = start.elapsed();
        let eval = Outcome::of(&expanded);
        let recorded = Call {
            krate: self.krate.clone(),
            name: self.name.clone(),
            kind: self.kind,
            calling,
            subtree: call.subtree.copy(),
            attrs: call.attrs.map(|it| it.copy()),
            env: call.env.clone(),
            def_site: call.def_site,
            call_site: call.call_site,
            mixed_site: call.mixed_site,
            current_dir: call.current_dir.to_owned(),
        };
        self.records.lock().push(Record { call: recorded, eval, took });
        expanded
    }
}

impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recorder")
            .field("krate", &self.krate)
            .field("name", &self.name)
            .field("eval", &self.eval)
            .finish()
    }
}

impl ProcMacroExpander for Recorder {
    fn requires_session(&self) -> bool {
        self.eval.requires_session()
    }

    fn ordered(&self) -> bool {
        self.eval.ordered()
    }

    fn speculates(&self, db: &Db, calling_crate: Crate) -> bool {
        matches!(self.ahead, Ahead::On(_)) && self.eval.speculates(db, calling_crate)
    }

    fn speculate(&self, db: &Db, calling_crate: Crate, call: ProcMacroCall<'_>) {
        if let Ahead::On(count) = &self.ahead {
            count.fetch_add(1, Ordering::Relaxed);
            self.eval.speculate(db, calling_crate, call);
        }
    }

    fn code(&self) -> Option<ContentId> {
        self.eval.code()
    }

    fn expand(
        &self,
        db: &Db,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        self.run(db, None, call)
    }

    fn expand_in(
        &self,
        db: &Db,
        calling_crate: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        self.run(db, Some(calling_crate), call)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any).downcast_ref::<Recorder>().is_some_and(|other| {
            Arc::ptr_eq(&self.records, &other.records)
                && self.krate == other.krate
                && self.name == other.name
                && self.kind == other.kind
        })
    }
}
