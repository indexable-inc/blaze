//! Calls whose macros panic, apart from `corpus-calls` so that it builds, among calls of the
//! same crate's macros that expand: each panic fails its own call alone.

corpus_macros::make_fn!(before: u32 = 1);
corpus_macros::boom!(a b c);
corpus_macros::make_fn!(between: u32 = 2);
corpus_macros::boom!();
corpus_macros::make_fn!(after: u32 = 3);
corpus_macros::make_fn!(last: u32 = 4);

mod bad {
    tokio_macros::select_priv_declare_output_enum!(nothing);
}
