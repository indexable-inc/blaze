//@ edition: 2021
pub trait Shape { const SIDES: u32; type Unit; fn unit(&self) -> Self::Unit; fn sides(&self) -> u32 { Self::SIDES } }
pub struct Tri;
pub struct Square;
impl Shape for Tri { const SIDES: u32 = 3; type Unit = &'static str; fn unit(&self) -> &'static str { "cm" } }
impl Shape for Square { const SIDES: u32 = 4; type Unit = u8; fn unit(&self) -> u8 { 1 } fn sides(&self) -> u32 { Self::SIDES * 1 } }
pub fn total<S: Shape>(shapes: &[S]) -> u32 { shapes.iter().map(|s| s.sides()).sum() }
