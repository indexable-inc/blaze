#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn call<F: FnMut()>(mut f: F) { f() }
pub fn call_fn<F: Fn()>(mut f: F) { f() }
pub fn pass<F: FnMut()>(mut f: F) { call(f) }
pub fn pass_ref<F: FnMut()>(mut f: F) { call(&mut f) }
pub fn once<F: FnOnce()>(mut f: F) { f() }
pub fn dyn_mut(mut f: Box<dyn FnMut()>) { f() }
pub fn dyn_fn(mut f: Box<dyn Fn()>) { f() }
pub fn ptr(mut f: fn()) { f() }
pub fn iter<I: Iterator<Item = u8>>(mut i: I) -> Option<u8> { i.next() }
pub fn iter_into<I: Iterator<Item = u8>>(mut i: I) -> u8 { i.sum() }
