pub trait Select {
    type Output;
    fn selected(&self) -> Self::Output;
}

pub use maker::make as reexported;
use maker::make as renamed_macro;

// Neither wrapper writes an impl itself. Their transitive proc-macro call does.
macro_rules! select_inner {
    () => {
        crate::reexported!()
    };
}
macro_rules! select {
    () => {
        select_inner!()
    };
}

pub fn selected() -> u8 {
    select!()
}

pub fn renamed() -> u8 {
    renamed_macro!()
}

pub fn local_alias() -> u8 {
    use maker::make as local_macro;
    local_macro!()
}

pub struct Global;
pub trait Flag {
    fn flag(&self) -> u8;
}

// An impl in a function is available outside that function, even if it is never called.
fn implementations() {
    maker::attach!();
}

pub fn global() -> u8 {
    Global.flag()
}
