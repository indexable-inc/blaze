//@ edition: 2021
use std::fmt::Display;
pub fn join_all(items: impl IntoIterator<Item = impl Display>) -> String { items.into_iter().map(|it| it.to_string()).collect::<Vec<_>>().join(",") }
pub fn evens() -> impl Iterator<Item = u32> { (0..10).step_by(2) }
