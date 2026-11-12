//! PROBE ONLY (branch launch3-1111-probe): where a launcher's start goes.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
pub static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
pub const N: usize = 24;
pub static ACC: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
pub static NAMES: [&str; N] = ["main:sidecar","main:connect","main:driver_new","main:spawn_control","ctl:loaded_new","ctl:begin(reader+hello)","ctl:wait_total","ctl:record(hash+copy)","ctl:statics","ctl:declare","ctl:edit_total","edit:records.code","edit:code.copy","edit:placed(register)","edit:settle","edit:stage","edit:commit+switch","ctl:start_sent_at","main:start_got_at","main:inits","main:entry","frames","frame_bytes","ctl:first_frame_at"];
pub fn now() -> u64 { T0.get_or_init(Instant::now).elapsed().as_nanos() as u64 }
pub fn add(i: usize, ns: u64) { ACC[i].fetch_add(ns, Ordering::Relaxed); }
pub fn set(i: usize, ns: u64) { ACC[i].store(ns, Ordering::Relaxed); }
pub struct Span(usize, u64);
pub fn span(i: usize) -> Span { Span(i, now()) }
impl Drop for Span { fn drop(&mut self) { add(self.0, now() - self.1); } }
pub fn dump() {
    if std::env::var_os("BLAZE_LAUNCH_TIMES").is_none() { return; }
    let mut s = String::new();
    for i in 0..N { s.push_str(&format!("{}={} ", NAMES[i], ACC[i].load(Ordering::Relaxed))); }
    eprintln!("TIMES {s}total_to_exit={}", now());
}
