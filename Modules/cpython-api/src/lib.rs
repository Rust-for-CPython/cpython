//! Safe Rust API for in-tree CPython extension modules.

mod c_api;
mod object;

pub use object::PyAny;
