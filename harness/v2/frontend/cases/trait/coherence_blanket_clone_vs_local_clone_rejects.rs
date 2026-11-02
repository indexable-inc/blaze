//@ expect: reject E0119
//@ edition: 2021
// A blanket impl over `T: Clone` overlaps an impl for a local type that is Clone.
#![crate_type = "lib"]

pub trait Tr {}

#[derive(Clone)]
pub struct Local;

impl<T: Clone> Tr for T {}
impl Tr for Local {}
