#![no_std]

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        mod x86_64;
        pub use self::x86_64::*;
    }
}

pub mod io;
mod lazy;

pub use self::lazy::Azy;
