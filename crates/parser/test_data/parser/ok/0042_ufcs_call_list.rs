// https://github.com/rust-lang/rust-analyzer/issues/596

struct Foo;

impl Foo {
    fn bar() -> bool {
        not_supported!()
    }
}

fn baz(_: bool) {}

fn main() {
    baz(<Foo>::bar())
}
