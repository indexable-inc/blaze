//! Automatic `Copy` / `Clone` for Blaze-edition transparent all-`pub` no-`Drop` structs
//! (`AdtFlags::AUTO_COPY_PUB`, builtin.rs `step_copy_clone`).

use crate::testing::parse::parse_file;
use crate::testing::run::{PRELUDE, Report, run_text};

const CASES: &str = r#"
extern impl Copy for u32
extern impl Clone for u32
extern impl Copy for i32
extern impl Clone for i32
extern impl<T: ?Sized> Copy for &T
extern impl<T: ?Sized> Clone for &T

struct NonCopy(u32)
struct OnlyClone(u32)
impl Clone for OnlyClone

case auto_copy_pub_concrete = builtin.rs::step_copy_clone
auto_copy_pub struct Pos(i32, i32)
auto_copy_pub struct Alive
auto_copy_pub struct Line(Pos, Pos)
goal Pos: Copy => yes
goal Pos: Clone => yes
goal Alive: Copy => yes
goal Alive: Clone => yes
goal Line: Copy => yes
goal Line: Clone => yes

case auto_copy_pub_generic = builtin.rs::step_copy_clone
auto_copy_pub struct Pair<T>(T, T)
auto_copy_pub struct RefWrap<T>(&T, u32)
goal Pair<i32>: Copy => yes
goal Pair<i32>: Clone => yes
goal Pair<NonCopy>: Copy => no
goal Pair<NonCopy>: Clone => no
goal Pair<OnlyClone>: Copy => no
goal Pair<OnlyClone>: Clone => yes
goal RefWrap<NonCopy>: Copy => yes
goal RefWrap<NonCopy>: Clone => yes

case auto_copy_pub_private_or_enum_non_copy = builtin.rs::step_copy_clone
struct PrivateField(i32, i32)
auto_copy_pub enum Tag { i32 | u32 }
goal PrivateField: Copy => no
goal PrivateField: Clone => no
goal Tag: Copy => no
goal Tag: Clone => no

case auto_copy_pub_has_drop_non_copy = builtin.rs::step_copy_clone
auto_copy_pub struct Guard(i32)
impl Drop for Guard
goal Guard: Copy => no
goal Guard: Clone => no

case auto_copy_pub_explicit_impl_wins = builtin.rs::step_copy_clone
auto_copy_pub struct CustomClone(i32)
impl Clone for CustomClone
auto_copy_pub struct CustomCopy(i32)
impl Copy for CustomCopy
impl Clone for CustomCopy
goal CustomClone: Clone => yes
goal CustomClone: Copy => yes
goal CustomCopy: Copy => yes
goal CustomCopy: Clone => yes

case auto_copy_pub_negative_impl_overrides = builtin.rs::step_copy_clone
auto_copy_pub struct BigBuf(i32, i32)
impl !Copy for BigBuf
auto_copy_pub struct NoClone(i32)
impl !Clone for NoClone
auto_copy_pub struct SpecPair<T>(T, T)
impl !Copy for SpecPair<i32>
goal BigBuf: Copy => no
goal BigBuf: Clone => yes
goal NoClone: Clone => no
goal NoClone: Copy => no
goal SpecPair<i32>: Copy => no
goal SpecPair<u32>: Copy => yes
"#;

#[test]
fn auto_copy_pub_solver_cases() {
    let prelude = parse_file(PRELUDE).expect("prelude parses");
    let mut report = Report::default();
    run_text("auto_copy_pub", &prelude, CASES, &|_| true, &mut report).expect("auto_copy_pub cases parse");
    assert_eq!(report.cases, 6, "expected 6 cases");
    assert!(report.failures.is_empty(), "failures: {:#?}", report.failures);
}
