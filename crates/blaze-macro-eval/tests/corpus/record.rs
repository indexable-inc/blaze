//! Each call of a proc macro as the front end made it, with what blaze-macro-eval's expander
//! gave (the output the front end goes on with).

use std::{
    any::Any,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use base_db::{ContentId, Crate, Db, Env};
use hir_expand::proc_macro::{ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind};
use parking_lot::Mutex;
use span::Span;
use tt::TopSubtree;

pub(crate) enum Outcome {
    Expanded(TopSubtree),
    Panicked(String),
    Failed(String),
}

impl Outcome {
    fn of(result: &Result<TopSubtree, ProcMacroExpansionError>) -> Outcome {
        match result {
            Ok(tree) => Outcome::Expanded(tree.copy()),
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
        let mut idents = self.subtree.as_token_trees().iter_flat_tokens().filter_map(|it| match it {
            tt::TokenTree::Leaf(tt::Leaf::Ident(ident)) => Some(ident.sym.as_str().to_owned()),
            _ => None,
        });
        if self.kind == ProcMacroKind::Bang {
            return idents.next();
        }
        let item = |it: &str| matches!(it, "struct" | "enum" | "union" | "fn" | "trait" | "type" | "mod");
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
            tt::TokenTree::Leaf(tt::Leaf::Literal(literal)) => Some(format!("{}{}", literal.text(), literal.suffix())),
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

pub(crate) type Records = Arc<Mutex<Vec<Record>>>;

/// blaze-macro-eval's expander of one macro, recording each call with what it gave.
pub(crate) struct Recorder {
    krate: Box<str>,
    name: Box<str>,
    kind: ProcMacroKind,
    eval: Arc<dyn ProcMacroExpander>,
    records: Records,
}

/// A call that panicked records nothing: the records kept are each whole.
impl std::panic::RefUnwindSafe for Recorder {}

impl Recorder {
    /// `eval`, of crate `krate`, recording into `records`.
    pub(crate) fn wrap(krate: &str, eval: ProcMacro, records: &Records) -> ProcMacro {
        let ProcMacro { name, kind, expander, disabled } = eval;
        let recorder = Recorder {
            krate: krate.into(),
            name: name.as_str().into(),
            kind,
            eval: expander,
            records: records.clone(),
        };
        ProcMacro { name, kind, expander: Arc::new(recorder), disabled }
    }

    fn run(&self, db: &Db, calling: Option<Crate>, call: ProcMacroCall<'_>) -> Result<TopSubtree, ProcMacroExpansionError> {
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
        f.debug_struct("Recorder").field("krate", &self.krate).field("name", &self.name).field("eval", &self.eval).finish()
    }
}

impl ProcMacroExpander for Recorder {
    fn requires_session(&self) -> bool {
        self.eval.requires_session()
    }

    fn code(&self) -> Option<ContentId> {
        self.eval.code()
    }

    fn expand(&self, db: &Db, call: ProcMacroCall<'_>) -> Result<TopSubtree, ProcMacroExpansionError> {
        self.run(db, None, call)
    }

    fn expand_in(&self, db: &Db, calling_crate: Crate, call: ProcMacroCall<'_>) -> Result<TopSubtree, ProcMacroExpansionError> {
        self.run(db, Some(calling_crate), call)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any)
            .downcast_ref::<Recorder>()
            .is_some_and(|other| {
                Arc::ptr_eq(&self.records, &other.records) && self.krate == other.krate && self.name == other.name && self.kind == other.kind
            })
    }
}
