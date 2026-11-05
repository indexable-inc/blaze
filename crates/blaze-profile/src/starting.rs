//! What a thread that starts something is inside now, read from another thread: the daemon's
//! start writes it on its ready pipe every 500 ms until it listens
//! (`blaze_cargo::serve::Ready`), so a start that never listens names the span it was stuck in
//! rather than only its phase.

use std::fmt::Write as _;

use crate::flight::{OpenSpan, threads_now};

/// The thread a start runs on, known by the operating system's id its flight ring is under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Starting {
    tid: u32,
}

impl Starting {
    /// The calling thread.
    pub fn here() -> Starting {
        // The flight ring keeps the low 32 bits of the operating system's id
        // (`flight::Carry`'s `tid`, `Ring::current`).
        Starting { tid: crate::sampler::os_tid() as u32 }
    }

    /// What the thread is inside now: its innermost open span its flight ring names, or `None`
    /// when it is inside none (or has no ring yet, having recorded no span). A thread moving
    /// while it is read may show a span it just left, as [`threads_now`] says.
    pub fn inside(&self) -> Option<Inside> {
        let thread = threads_now().into_iter().find(|it| it.tid == u64::from(self.tid))?;
        let deeper = thread.deeper;
        let span = thread.open.into_iter().next_back()?;
        Some(Inside { span, deeper })
    }
}

/// A thread's innermost open span, as [`Starting::inside`] read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inside {
    pub span: OpenSpan,
    /// Spans it is inside past the deepest its ring names: `span` is then not the innermost.
    pub deeper: u32,
}

/// "`open store` (crate=std, awaits=..., 59.8s open)": the span's name, its fields that are
/// set, and how long it has been open.
impl std::fmt::Display for Inside {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let span = &self.span;
        // alloc: one line every 500 ms of a daemon's start
        let mut fields = String::new();
        let named = [
            ("crate", span.krate.as_deref()),
            ("function", span.function.as_deref()),
            ("unit", span.unit.as_deref()),
            ("awaits", span.awaits.as_deref()),
        ];
        for (key, value) in named {
            if let Some(value) = value {
                // Writing to a `String` does not fail.
                _ = write!(fields, "{key}={value}, ");
            }
        }
        if let Some(owner) = span.owner {
            _ = write!(fields, "owner={owner}, ");
        }
        write!(f, "`{}` ({fields}{:.1}s open", span.name, span.for_ns as f64 / 1e9)?;
        if self.deeper > 0 {
            write!(f, ", and {} spans deeper its flight ring does not name", self.deeper)?;
        }
        f.write_str(")")
    }
}
