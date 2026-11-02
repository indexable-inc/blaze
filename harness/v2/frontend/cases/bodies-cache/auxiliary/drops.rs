//@ edition: 2021
pub struct Noisy(pub &'static str);
impl Drop for Noisy { fn drop(&mut self) { println!("drop {}", self.0); } }
pub fn scoped() { let _a = Noisy("a"); { let _b = Noisy("b"); } let _c = Noisy("c"); println!("end of scoped"); }
pub fn moved() -> Noisy { let n = Noisy("moved"); n }
