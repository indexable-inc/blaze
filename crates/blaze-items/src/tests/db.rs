//! The tests' database: the build's [`base_db::Db`], made for tests
//! ([`base_db::Db::for_tests`]).

// Links `hir-ty-mir`, whose providers answer the MIR queries the hashes read.
use hir_ty_mir as _;

pub(crate) type TestDB = base_db::Db;
