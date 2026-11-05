//@ edition: 2021
//@ aux-crate: pm=procmac.rs
pub fn answer() -> u32 { pm::make_answer!() + 0 }
#[derive(pm::Named)]
pub struct Widget;
pub fn widget_name() -> &'static str { Widget::name() }
