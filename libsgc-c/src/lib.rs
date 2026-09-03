//! C ABI shim over the libsgc Rust core (`libsgc-rs`).
//!
//! Builds `libsgc.a` (static) and `libsgc.so` (shared) exporting the
//! `sgc_*` ABI declared in `include/libsgc.h`. There is no protocol logic
//! here — every function is a thin, panic-safe wrapper over the pump core
//! in `libsgc_rs`. C++ consumers wrap this ABI in `include/sgc.hpp`.
//!
//! Crate name is `libsgc-c`; the library name is `sgc` (link with `-lsgc`).

pub mod ffi;
