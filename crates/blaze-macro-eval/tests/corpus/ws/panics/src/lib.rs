//! Calls whose macros panic, apart from `corpus-calls` so that it builds.

corpus_macros::boom!(a b c);
corpus_macros::boom!();

mod bad {
    tokio_macros::select_priv_declare_output_enum!(nothing);
}
