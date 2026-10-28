// clang-sys includes its target macros and search modules in a function emitted by link!.
// The include must expand before child modules inherit the parent's textual macro scope.
fn direct() -> (u32, u32, u32) {
    mod build {
        include!("macros.rs");
        pub mod common {
            include!("common.rs");
        }
        pub mod dynamic {
            include!("dynamic.rs");
        }
    }

    (build::common::found(), build::dynamic::found(), build::dynamic::nested::found())
}

macro_rules! declare_loader {
    ($name:ident) => {
        fn $name() -> (u32, u32, u32) {
            mod build {
                include!("macros.rs");
                pub mod common {
                    include!("common.rs");
                }
                pub mod dynamic {
                    include!("dynamic.rs");
                }
            }

            (build::common::found(), build::dynamic::found(), build::dynamic::nested::found())
        }
    };
}

declare_loader!(generated);

fn main() {
    let expected = (42, 43, 44);
    assert_eq!(direct(), expected);
    assert_eq!(generated(), expected);
    println!("{:?} {:?}", direct(), generated());
}
