// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#![cfg_attr(feature = "strict", deny(clippy:all))]
#![recursion_limit = "512"]
#![feature(atomic_from_mut)]
#![feature(never_type)]
#![feature(test)]
#![feature(type_alias_impl_trait)]
#![feature(allocator_api)]
#![feature(slice_ptr_get)]
#![feature(strict_provenance)]
#![cfg_attr(target_os = "windows", feature(maybe_uninit_uninit_array))]

mod collections;
mod pal;

#[cfg(feature = "profiler")]
pub mod perftools;

pub mod scheduler;

pub mod runtime;

pub mod inetstack;

extern crate test;

#[macro_use]
extern crate log;

#[macro_use]
extern crate cfg_if;

#[cfg(feature = "catnip-libos")]
pub mod catnip;

#[cfg(feature = "catpowder-libos")]
mod catpowder;

#[cfg(feature = "catcollar-libos")]
mod catcollar;

#[cfg(all(feature = "catnap-libos", target_os = "linux"))]
mod catnap;

#[cfg(all(feature = "catnapw-libos", target_os = "windows"))]
mod catnapw;

#[cfg(feature = "catmem-libos")]
mod catmem;

#[cfg(feature = "catloop-libos")]
mod catloop;

pub use self::demikernel::libos::{
    name::LibOSName,
    LibOS,
};
pub use crate::runtime::{
    network::types::{
        MacAddress,
        Port16,
    },
    types::{
        demi_sgarray_t,
        demi_sgaseg_t,
    },
    OperationResult,
    QDesc,
    QResult,
    QToken,
    QType,
};

pub mod demikernel;

//======================================================================================================================
// Macros
//======================================================================================================================

/// Ensures that two expressions are equivalent or return an Error.
#[macro_export]
macro_rules! ensure_eq {
    ($left:expr, $right:expr) => ({
        match (&$left, &$right) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    anyhow::bail!(r#"ensure failed: `(left == right)` left: `{:?}`,right: `{:?}`"#, left_val, right_val)
                }
            }
        }
    });
    ($left:expr, $right:expr,) => ({
        crate::ensure_eq!($left, $right)
    });
    ($left:expr, $right:expr, $($arg:tt)+) => ({
        match (&($left), &($right)) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    anyhow::bail!(r#"ensure failed: `(left == right)` left: `{:?}`, right: `{:?}`: {}"#, left_val, right_val, format_args!($($arg)+))
                }
            }
        }
    });
}

/// Ensure that two expressions are not equal or returns an Error.
#[macro_export]
macro_rules! ensure_neq {
    ($left:expr, $right:expr) => ({
        match (&$left, &$right) {
            (left_val, right_val) => {
                if (*left_val == *right_val) {
                    anyhow::bail!(r#"ensure failed: `(left == right)` left: `{:?}`,right: `{:?}`"#, left_val, right_val)
                }
            }
        }
    });
    ($left:expr, $right:expr,) => ({
        crate::ensure_neq!($left, $right)
    });
    ($left:expr, $right:expr, $($arg:tt)+) => ({
        match (&($left), &($right)) {
            (left_val, right_val) => {
                if (*left_val == *right_val) {
                    anyhow::bail!(r#"ensure failed: `(left == right)` left: `{:?}`, right: `{:?}`: {}"#, left_val, right_val, format_args!($($arg)+))
                }
            }
        }
    });
}

#[test]
fn test_ensure() -> Result<(), anyhow::Error> {
    ensure_eq!(1, 1);
    ensure_eq!(1, 1, "test");
    ensure_neq!(1, 2);
    ensure_neq!(1, 2, "test");
    Ok(())
}
