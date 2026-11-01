mod lang;
mod profiles;
mod types;

pub use lang::{Lang, detect, detect_from_extension};
pub use types::Profile;

#[cfg(test)]
mod tests;
