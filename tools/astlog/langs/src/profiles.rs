use crate::{lang::Lang, types::Profile};

mod compiled;
mod config;
mod scripting;

pub(crate) fn get(lang: Lang) -> &'static Profile {
    match lang {
        Lang::Rust => &compiled::RUST,
        Lang::Toml => &config::TOML,
        Lang::Python => &scripting::PYTHON,
    }
}
