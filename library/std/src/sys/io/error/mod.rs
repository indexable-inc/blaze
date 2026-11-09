cfg_select! {
    target_os = "hermit" => {
        mod hermit;
        pub use hermit::*;
    }
    target_os = "motor" => {
        mod motor;
        pub use motor::*;
    }
    all(target_vendor = "fortanix", target_env = "sgx") => {
        mod sgx;
        pub use sgx::*;
    }
    target_os = "solid_asp3" => {
        mod solid;
        pub use solid::*;
    }
    target_os = "uefi" => {
        mod uefi;
        pub use uefi::*;
    }
    any(target_family = "unix", target_os = "wasi", target_os = "teeos") => {
        mod unix;
        pub use unix::*;
    }
    target_os = "windows" => {
        mod windows;
        pub use windows::*;
    }
    target_os = "xous" => {
        mod xous;
        pub use xous::*;
    }
    // blaze: `target_arch = "naga"`, the GPU device target (naga-unknown-unknown).
    any(target_os = "vexos", target_family = "wasm", target_os = "zkvm", target_os = "trusty", target_arch = "naga") => {
        mod generic;
        pub use generic::*;
    }
}
