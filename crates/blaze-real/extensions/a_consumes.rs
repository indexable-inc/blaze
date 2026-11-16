//! consumes: `#[blaze::consumes]` ends a linear value; `main` hands its value to it. A working layer accepts it.
//! expect: accept

#[blaze::linear]
struct Txn {
    id: i32,
}

impl Txn {
    #[blaze::consumes]
    fn commit(self) -> i32 {
        self.id
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let txn = Txn { id: 42 };
    txn.commit()
}
