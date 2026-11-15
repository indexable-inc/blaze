//! A stand-in for `crates/blaze-mem`'s `frame` module: the two functions the rule names, with
//! their real signatures.

pub mod frame {
    pub struct Frame;
    pub struct Tag(pub &'static str);

    /// Runs `f` in a task with a frame of its own.
    pub fn task<R>(tag: Tag, f: impl FnOnce(&Frame) -> R) -> R {
        let _ = tag;
        f(&Frame)
    }

    /// Runs `f` with the innermost task's frame; panics outside any task.
    pub fn current<R>(f: impl FnOnce(&Frame) -> R) -> R {
        f(&Frame)
    }
}
