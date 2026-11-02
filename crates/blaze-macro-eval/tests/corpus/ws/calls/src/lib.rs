//! The calls the corpus expands (`crates/blaze-macro-eval/tests/corpus.rs`), each at item
//! level, so that resolving the crate's names expands every one of them. Its tests (`tests.rs`)
//! say what each expansion has to do, built with the expansions the build made.

#[cfg(test)]
mod tests;

use corpus_macros::{env_name, include_data, label, make_fn, Describe};
use corpus_state::counter;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Plain {
    pub a: u32,
    pub b: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Renamed {
    #[serde(rename = "id")]
    pub user_id: u64,
    #[serde(skip)]
    pub cache: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_note: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Outer {
    pub name: String,
    #[serde(flatten)]
    pub inner: Plain,
}

#[derive(Serialize, Deserialize)]
pub struct Wrapper<T: Clone, const N: usize> {
    pub value: T,
    pub items: Vec<T>,
    #[serde(skip)]
    pub marker: std::marker::PhantomData<[u8; N]>,
}

#[derive(Serialize, Deserialize)]
pub struct Borrowed<'a, 'b: 'a> {
    #[serde(borrow)]
    pub text: &'a str,
    #[serde(borrow)]
    pub more: std::borrow::Cow<'b, str>,
    pub n: i32,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Shape {
    Circle { r: f64 },
    #[serde(rename = "box")]
    Square { side: u32 },
    Point,
}

#[derive(Serialize, Deserialize)]
pub struct Pair(pub u8, #[serde(skip)] pub u16);

#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad value {value} at {at:?}")]
    Bad { value: i64, at: usize },
    #[error("wrapped")]
    Wrapped {
        #[source]
        cause: std::fmt::Error,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("parse error on line {line}")]
pub struct ParseError {
    pub line: u32,
}

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct Opaque(#[from] std::num::ParseIntError);

#[derive(Debug, thiserror::Error)]
#[error("{name} of {}", .source)]
pub struct Generic<E: std::error::Error + 'static> {
    pub name: &'static str,
    #[source]
    pub source: E,
}

#[derive(Describe)]
pub struct Described {
    pub a: u8,
    #[describe(skip)]
    pub hidden: u16,
    pub pairs: Vec<(u8, std::collections::BTreeMap<u8, u8>)>,
    pub call: fn(u8) -> u8,
}

#[label("first")]
pub fn labelled() -> u32 {
    1
}

#[label("second")]
pub fn labelled_again(x: u32) -> u32 {
    x + 1
}

/// What `tokio_macros::main` and `test` expand to call, in place of tokio's runtime, which the
/// package does not depend on.
pub mod rt {
    pub mod runtime {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        pub struct Builder;

        pub struct Runtime;

        impl Builder {
            pub fn new_current_thread() -> Builder {
                Builder
            }

            pub fn enable_all(&mut self) -> &mut Builder {
                self
            }

            pub fn build(&mut self) -> std::io::Result<Runtime> {
                Ok(Runtime)
            }
        }

        impl Runtime {
            pub fn block_on<F: Future>(&self, future: F) -> F::Output {
                let mut future = std::pin::pin!(future);
                let mut cx = Context::from_waker(Waker::noop());
                loop {
                    if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                        return output;
                    }
                }
            }
        }
    }

    macro_rules! pin {
        ($($name:ident),*) => {
            $(let $name = ::core::pin::pin!($name);)*
        };
    }
    pub(crate) use pin;
}

#[tokio_macros::main(crate = "crate::rt", flavor = "current_thread")]
pub async fn tokio_main() {}

#[tokio_macros::test(crate = "crate::rt")]
async fn tokio_test() {}

make_fn!(answer: u32 = 42);
make_fn!(greeting: &'static str = "hi");

mod select {
    tokio_macros::select_priv_declare_output_enum!((_ _ _));
}

include_data!(DATA, "data.txt");
env_name!(PACKAGE);
counter!(FIRST);
counter!(SECOND);
