//@ edition: 2021
pub fn broken() -> usize { let mut v = vec![1]; let a = &mut v; let b = &mut v; a.push(2); b.len() }
