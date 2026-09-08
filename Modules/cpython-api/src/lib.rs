//! Safe Rust API for CPython.
//!
//! See `RUST_API.md` at the repository root for the design document. The short
//! version:
//!
//! - The thread state is passed explicitly everywhere as [`ThreadState`],
//!   which wraps the actual `PyThreadState*` rather than being a zero-sized
//!   token. Every operation that touches the interpreter requires
//!   `&ThreadState`; [`ThreadState::detach`] takes `&mut self` so nothing can
//!   touch the interpreter while detached.
//! - PyO3 vocabulary is used where the semantics match: [`Bound`], [`Py`],
//!   [`PyResult`], [`types::PyBytes`], ...
//! - Errors are represented by the zero-sized [`PyErrRaised`] marker: the
//!   exception itself lives in the thread state, exactly as in the C API.
//! - Modules are defined with PEP 793 (`PyModExport`) via [`export_module!`];
//!   classes with PEP 820 (`PyType_FromSlots`) via [`class::ClassDef`]. Both
//!   are free-threading and subinterpreter compatible by default.

pub use cpython_sys as ffi;

pub mod args;
pub mod buffer;
pub mod class;
pub mod conversion;
pub mod err;
pub mod instance;
pub mod module;
pub(crate) mod sys_calls;
pub mod threadstate;
pub mod types;

pub use buffer::PyBuffer;
pub use conversion::{FromPyObject, IntoPyObject};
pub use err::{PyErrRaised, PyResult};
pub use instance::{Bound, Py};
pub use module::{ModuleState, TraverseResult, TraverseStop, Visit};
pub use threadstate::ThreadState;
pub use types::{PyAny, PyBytes, PyModule, PyType};

pub use cpython_api_macros::{pyfunction, pymethods};

/// Commonly used items, for glob import in module implementations.
pub mod prelude {
    pub use crate::buffer::PyBuffer;
    pub use crate::conversion::{FromPyObject, IntoPyObject};
    pub use crate::err::{
        PyBufferError, PyEOFError, PyException, PyMemoryError, PyNotImplementedError,
        PyOverflowError, PyRuntimeError, PySystemError, PyTypeError, PyValueError,
    };
    pub use crate::err::{PyErrRaised, PyResult};
    pub use crate::instance::{Bound, Py};
    pub use crate::module::ModuleState;
    pub use crate::threadstate::ThreadState;
    pub use crate::types::{PyAny, PyBytes, PyModule, PyType};
    pub use crate::{export_module, pyfunction, pymethods};
}

/// Build a `&'static CStr` from a nul-terminated byte string at compile time.
///
/// Used by macro-generated code; panics at compile time if `bytes` is not
/// nul-terminated or contains interior nul bytes.
#[doc(hidden)]
pub const fn const_cstr(bytes: &'static [u8]) -> &'static core::ffi::CStr {
    match core::ffi::CStr::from_bytes_with_nul(bytes) {
        Ok(c) => c,
        Err(_) => panic!("invalid C string literal"),
    }
}
