//! blaze: upstream's profiler reads Z3's trace with smt-scope; blaze keeps only the quantifier
//! name prefixes VIR writes (`vir::def`). A profile of a proof is a blaze span instead.

pub const USER_QUANT_PREFIX: &str = "user_";
pub const INTERNAL_QUANT_PREFIX: &str = "internal_";
