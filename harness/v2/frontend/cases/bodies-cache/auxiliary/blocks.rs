//@ edition: 2021
pub fn with_inner() -> u32 {
    fn helper(x: u32) -> u32 { x * 10 }
    struct Local(u32);
    impl Local { fn get(&self) -> u32 { self.0 + 1 } }
    const K: u32 = 4;
    helper(Local(K).get())
}
pub fn block_value() -> i32 { let x = { let y = 3; y * y }; x - 1 }
