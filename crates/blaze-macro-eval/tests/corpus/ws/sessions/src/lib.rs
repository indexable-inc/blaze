//! The calls of the corpus's macros that keep a session, three of each at item level, in a
//! crate of their own so the other tests' calls are not in their sessions (`tests/kept.rs`).

use corpus_once::once;
use corpus_settles::detect;

once!(ONCE_FIRST);
once!(ONCE_SECOND);
once!(ONCE_THIRD);

detect!(DETECT_FIRST);
detect!(DETECT_SECOND);
detect!(DETECT_THIRD);
