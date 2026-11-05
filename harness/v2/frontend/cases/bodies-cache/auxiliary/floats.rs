//@ edition: 2021
pub fn hyp(a: f64, b: f64) -> f64 { (a * a + b * b).sqrt() }
pub fn mean(xs: &[f32]) -> f32 { xs.iter().sum::<f32>() / xs.len() as f32 }
pub fn round_trip(x: f64) -> i64 { (x * 100.0).round() as i64 }
