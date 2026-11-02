//! The tests' database: the build's [`base_db::Db`], made for tests
//! ([`base_db::Db::for_tests`]).

// `hir` links `hir-ty-mir`, whose providers answer the MIR queries the hashes read.
use hir as _;

pub(crate) type TestDB = base_db::Db;
