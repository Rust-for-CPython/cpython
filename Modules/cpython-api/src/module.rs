//! Module definition via PEP 793 (`PyModExport` + `PySlot` arrays).
//!
//! Modules are defined with the [`export_module!`] macro, which expands to a
//! static slot array and the `PyModExport_<name>` entry point. Free-threading
//! (`Py_mod_gil = Py_MOD_GIL_NOT_USED`) and subinterpreter support
//! (`Py_mod_multiple_interpreters = PER_INTERPRETER_GIL_SUPPORTED`) are
//! emitted unconditionally — they are defaults of this API, not options.
//!
//! Per-module state (the [`ModuleState`] trait) is where exception types and
//! class type objects live, so modules never share objects across
//! interpreters through statics.

use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_void};

use crate::err::{PyResult, PySystemError};
use crate::ffi;
use crate::instance::{Bound, Py};
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::PyModule;

// Slot IDs that are function-like compat macros in C (`_Py_SLOT_COMPAT_VALUE`)
// and therefore absent from the generated bindings. Values are the non-
// limited-API (3.15+) IDs from Include/slots_generated.h.
pub const PY_MOD_CREATE: u32 = 84;
pub const PY_MOD_EXEC: u32 = 85;
pub const PY_MOD_MULTIPLE_INTERPRETERS: u32 = 86;
pub const PY_MOD_GIL: u32 = 87;

// Pointer-valued slot values (pointer-cast macros in C, absent from bindings).
pub const PY_MOD_MULTIPLE_INTERPRETERS_SUPPORTED: *mut c_void = std::ptr::without_provenance_mut(1);
pub const PY_MOD_PER_INTERPRETER_GIL_SUPPORTED: *mut c_void = std::ptr::without_provenance_mut(2);
pub const PY_MOD_GIL_NOT_USED: *mut c_void = std::ptr::without_provenance_mut(1);

// --- PySlot construction ---------------------------------------------------

/// `PySlot_END`.
pub const SLOT_END: ffi::PySlot = ffi::PySlot {
    sl_id: 0,
    sl_flags: 0,
    __bindgen_anon_1: ffi::PySlot__bindgen_ty_1 { sl_reserved: 0 },
    __bindgen_anon_2: ffi::PySlot__bindgen_ty_2 {
        sl_ptr: std::ptr::null_mut(),
    },
};

/// `PySlot_DATA`: a data-pointer slot.
pub const fn slot_data(id: u32, ptr: *mut c_void) -> ffi::PySlot {
    ffi::PySlot {
        sl_id: id as u16,
        sl_flags: ffi::PySlot_INTPTR as u16,
        __bindgen_anon_1: ffi::PySlot__bindgen_ty_1 { sl_reserved: 0 },
        __bindgen_anon_2: ffi::PySlot__bindgen_ty_2 { sl_ptr: ptr },
    }
}

/// `PySlot_STATIC_DATA`: a data-pointer slot whose target is static.
pub const fn slot_static_data(id: u32, ptr: *mut c_void) -> ffi::PySlot {
    ffi::PySlot {
        sl_id: id as u16,
        sl_flags: ffi::PySlot_STATIC as u16,
        __bindgen_anon_1: ffi::PySlot__bindgen_ty_1 { sl_reserved: 0 },
        __bindgen_anon_2: ffi::PySlot__bindgen_ty_2 { sl_ptr: ptr },
    }
}

/// `PySlot_FUNC`: a function-pointer slot.
pub const fn slot_func(id: u32, func: ffi::_Py_funcptr_t) -> ffi::PySlot {
    ffi::PySlot {
        sl_id: id as u16,
        sl_flags: 0,
        __bindgen_anon_1: ffi::PySlot__bindgen_ty_1 { sl_reserved: 0 },
        __bindgen_anon_2: ffi::PySlot__bindgen_ty_2 { sl_func: func },
    }
}

/// `PySlot_SIZE`: a `Py_ssize_t` slot.
pub const fn slot_size(id: u32, size: ffi::Py_ssize_t) -> ffi::PySlot {
    ffi::PySlot {
        sl_id: id as u16,
        sl_flags: 0,
        __bindgen_anon_1: ffi::PySlot__bindgen_ty_1 { sl_reserved: 0 },
        __bindgen_anon_2: ffi::PySlot__bindgen_ty_2 { sl_size: size },
    }
}

/// `PySlot_UINT64`: a `uint64_t` slot.
pub const fn slot_uint64(id: u32, value: u64) -> ffi::PySlot {
    ffi::PySlot {
        sl_id: id as u16,
        sl_flags: 0,
        __bindgen_anon_1: ffi::PySlot__bindgen_ty_1 { sl_reserved: 0 },
        __bindgen_anon_2: ffi::PySlot__bindgen_ty_2 { sl_uint64: value },
    }
}

/// A static `PySlot` array.
///
/// SAFETY of `Sync`: the array is logically immutable after const
/// construction, and every pointer stored in it targets `'static` data
/// (strings, method tables, glue functions). The interpreter only reads it.
#[repr(transparent)]
pub struct SlotArray<const N: usize>(pub [ffi::PySlot; N]);

unsafe impl<const N: usize> Sync for SlotArray<N> {}

impl<const N: usize> SlotArray<N> {
    /// The pointer handed to `PyModExport`/`PyType_FromSlots`.
    ///
    /// The cast to `*mut` matches the C signatures; the interpreter treats
    /// the array as read-only.
    pub fn as_ptr(&'static self) -> *mut ffi::PySlot {
        self.0.as_ptr().cast_mut()
    }
}

/// A static, null-terminated `PyGetSetDef` array (emitted by `#[pymethods]`).
///
/// SAFETY of `Sync`: logically immutable, pointers target `'static` data.
#[repr(transparent)]
pub struct GetSetDefs<const N: usize>(pub [ffi::PyGetSetDef; N]);

unsafe impl<const N: usize> Sync for GetSetDefs<N> {}

/// A static, null-terminated `PyMethodDef` array (emitted by `#[pymethods]`;
/// module-level arrays are built by `export_module!`).
#[repr(transparent)]
pub struct MethodDefs<const N: usize>(pub [ffi::PyMethodDef; N]);

// PyMethodDef is Send + Sync in cpython-sys, so MethodDefs is automatically.

/// `PY_VERSION_HEX`, reassembled from its parts (the C macro is a
/// function-like macro invocation, which bindgen cannot evaluate).
pub const PY_VERSION_HEX: u32 = (ffi::PY_MAJOR_VERSION << 24)
    | (ffi::PY_MINOR_VERSION << 16)
    | (ffi::PY_MICRO_VERSION << 8)
    | (ffi::PY_RELEASE_LEVEL << 4)
    | ffi::PY_RELEASE_SERIAL;

/// The ABI declaration for a slots-only module built by this crate:
/// internal ABI (we link against `Py_BUILD_CORE` bindings), exact-version,
/// free-threaded or GIL to match the interpreter build.
pub const fn abi_info() -> ffi::PyABIInfo {
    let ft_flag = if ffi::GIL_DISABLED {
        ffi::PyABIInfo_FREETHREADED
    } else {
        ffi::PyABIInfo_GIL
    };
    ffi::PyABIInfo {
        abiinfo_major_version: 1,
        abiinfo_minor_version: 0,
        flags: (ffi::PyABIInfo_INTERNAL | ft_flag) as u16,
        build_version: PY_VERSION_HEX,
        abi_version: PY_VERSION_HEX,
    }
}

// --- typed per-module state ------------------------------------------------

/// GC visitor passed to [`ModuleState::traverse`].
pub struct Visit<'a> {
    visit: ffi::visitproc,
    arg: *mut c_void,
    _marker: PhantomData<&'a ()>,
}

/// Nonzero return from a `visitproc`, propagated out of `traverse`.
pub struct TraverseStop(pub c_int);

pub type TraverseResult = Result<(), TraverseStop>;

impl Visit<'_> {
    /// Report an owned object reference to the GC.
    pub fn call<T>(&self, obj: &Py<T>) -> TraverseResult {
        let Some(visit) = self.visit else {
            return Ok(());
        };
        let ret = unsafe { visit(obj.as_ptr(), self.arg) };
        if ret == 0 {
            Ok(())
        } else {
            Err(TraverseStop(ret))
        }
    }
}

/// Typed per-module state.
///
/// This is where a module keeps its exception types and class type objects
/// (as [`Py`] handles), mirroring the C convention (e.g. `zlibstate`).
/// Implementations holding `Py` references must implement `traverse` (and
/// usually `clear`, which requires those fields to be `Option`s so the
/// references can be dropped).
pub trait ModuleState: Sized + Send + Sync + 'static {
    /// Build the state. Runs from the `Py_mod_exec` glue, before the user
    /// exec function.
    ///
    /// Note the shared `'py`: `ThreadState` is invariant in its lifetime, so
    /// the token and the module reference must name the same attached scope.
    fn new<'py>(ts: &ThreadState<'py>, module: &Bound<'py, PyModule>) -> PyResult<Self>;

    /// Report owned object references to the GC.
    fn traverse(&self, _visit: &Visit<'_>) -> TraverseResult {
        Ok(())
    }

    /// Drop owned object references (GC cycle breaking).
    fn clear(&mut self) {}
}

/// The in-memory layout of the module state block.
///
/// CPython zero-fills the state allocation, so `initialized == false` until
/// the exec glue writes the value; the traverse/clear/free glue must not
/// touch `value` before then (exec can fail, and `m_free` runs regardless).
#[repr(C)]
pub struct StateStorage<T> {
    initialized: bool,
    value: MaybeUninit<T>,
}

/// `Py_mod_state_size` value for a state type.
pub const fn state_size<T: ModuleState>() -> ffi::Py_ssize_t {
    std::mem::size_of::<StateStorage<T>>() as ffi::Py_ssize_t
}

/// Borrow the initialized state out of a raw storage block (shared by the
/// module-pointer and defining-class access paths).
///
/// # Safety
///
/// `storage` must point to a live `StateStorage<T>` block.
#[doc(hidden)]
pub unsafe fn storage_state_ref<'a, T: ModuleState>(
    ts: &ThreadState<'_>,
    storage: *mut StateStorage<T>,
) -> PyResult<&'a T> {
    match unsafe { storage.as_ref() } {
        Some(storage) if storage.initialized => Ok(unsafe { storage.value.assume_init_ref() }),
        _ => Err(PySystemError::raise(ts, "module state not initialized")),
    }
}

unsafe fn storage_of<'a, T: ModuleState>(
    module: *mut ffi::PyObject,
) -> Option<&'a mut StateStorage<T>> {
    let ptr = unsafe { sys_calls::module_get_state(module) } as *mut StateStorage<T>;
    unsafe { ptr.as_mut() }
}

/// Borrow the typed state of `module`.
///
/// Raises `SystemError` if the module has no (initialized) state — which for
/// a module defined with [`export_module!`] means `T` doesn't match the
/// module's declared state type.
pub fn module_state<'a, T: ModuleState>(
    ts: &ThreadState<'_>,
    module: &'a Bound<'_, PyModule>,
) -> PyResult<&'a T> {
    unsafe { state_from_module_ptr(ts, module.as_ptr()) }
}

/// Borrow the typed state from a raw module pointer (trampoline glue).
///
/// # Safety
///
/// `module` must be a valid module object pointer that stays alive for `'a`,
/// and its state block must be a `StateStorage<T>` (guaranteed when the
/// module was defined by `export_module!` with state type `T`).
#[doc(hidden)]
pub unsafe fn state_from_module_ptr<'a, T: ModuleState>(
    ts: &ThreadState<'_>,
    module: *mut ffi::PyObject,
) -> PyResult<&'a T> {
    match unsafe { storage_of::<T>(module) } {
        Some(storage) if storage.initialized => Ok(unsafe { storage.value.assume_init_ref() }),
        _ => Err(PySystemError::raise(ts, "module state not initialized")),
    }
}

/// The user exec function type: shared `'py` between token and module.
pub type ModuleExecFn = for<'py> fn(&ThreadState<'py>, &Bound<'py, PyModule>) -> PyResult<()>;

/// `Py_mod_exec` glue: initialize the typed state, then run the user exec.
///
/// # Safety
///
/// Must only be called by the interpreter as the module's exec slot, with the
/// module created from an `export_module!` slot array declaring state type
/// `T`.
#[doc(hidden)]
pub unsafe fn exec_impl<T: ModuleState>(
    module: *mut ffi::PyObject,
    user_exec: ModuleExecFn,
) -> c_int {
    let ts = unsafe { ThreadState::current_unchecked() };
    let Some(ptr) = std::ptr::NonNull::new(module) else {
        let _ = PySystemError::raise(&ts, "module exec called with NULL module");
        return -1;
    };
    let bound: &Bound<'_, PyModule> = unsafe { Bound::ref_from_ptr(&ptr) };

    let Some(storage) = (unsafe { storage_of::<T>(module) }) else {
        let _ = PySystemError::raise(&ts, "module has no state block");
        return -1;
    };
    match T::new(&ts, bound) {
        Ok(state) => {
            storage.value.write(state);
            storage.initialized = true;
        }
        Err(_raised) => return -1,
    }

    match user_exec(&ts, bound) {
        Ok(()) => 0,
        Err(_raised) => -1,
    }
}

/// `Py_mod_state_traverse` glue.
///
/// # Safety
///
/// Interpreter-only entry point; see `exec_impl`.
#[doc(hidden)]
pub unsafe extern "C" fn traverse_impl<T: ModuleState>(
    module: *mut ffi::PyObject,
    visit: ffi::visitproc,
    arg: *mut c_void,
) -> c_int {
    match unsafe { storage_of::<T>(module) } {
        Some(storage) if storage.initialized => {
            let v = Visit {
                visit,
                arg,
                _marker: PhantomData,
            };
            match unsafe { storage.value.assume_init_ref() }.traverse(&v) {
                Ok(()) => 0,
                Err(TraverseStop(code)) => code,
            }
        }
        _ => 0,
    }
}

/// `Py_mod_state_clear` glue.
///
/// # Safety
///
/// Interpreter-only entry point; see `exec_impl`.
#[doc(hidden)]
pub unsafe extern "C" fn clear_impl<T: ModuleState>(module: *mut ffi::PyObject) -> c_int {
    if let Some(storage) = unsafe { storage_of::<T>(module) }
        && storage.initialized
    {
        unsafe { storage.value.assume_init_mut() }.clear();
    }
    0
}

/// `Py_mod_state_free` glue: drop the state in place.
///
/// Runs with an attached thread state (module deallocation), so `Py<T>`
/// fields decref normally.
///
/// # Safety
///
/// Interpreter-only entry point; see `exec_impl`.
#[doc(hidden)]
pub unsafe extern "C" fn free_impl<T: ModuleState>(module: *mut c_void) {
    if let Some(storage) = unsafe { storage_of::<T>(module as *mut ffi::PyObject) }
        && storage.initialized
    {
        storage.initialized = false;
        unsafe { storage.value.assume_init_drop() };
    }
}

// --- the export macro ------------------------------------------------------

/// Define a PEP 793 module: static slot array + `PyModExport_<name>` symbol.
///
/// ```ignore
/// export_module! {
///     name: zlib,
///     doc: c"zlib compression / decompression module",
///     state: ZlibState,
///     methods: [adler32, crc32, compress, decompress],
///     exec: zlib_exec,
/// }
/// ```
///
/// - `name` sets both the module name and the export symbol
///   (`PyModExport_zlib`). A Rust item `PyModExport` is also emitted at the
///   call site so static builds can re-export it
///   (`pub use my_module::PyModExport;`).
/// - `state` is a type implementing [`ModuleState`]; its `new` runs before
///   `exec`.
/// - `methods` lists `#[pyfunction]` names.
/// - `exec` is `fn(&ThreadState<'_>, &Bound<'_, PyModule>) -> PyResult<()>`.
///
/// The expansion always declares `Py_mod_gil = Py_MOD_GIL_NOT_USED` and
/// `Py_mod_multiple_interpreters = PER_INTERPRETER_GIL_SUPPORTED`: modules
/// built with this API must keep their state per-module and thread-safe.
#[macro_export]
macro_rules! export_module {
    (
        name: $name:ident,
        doc: $doc:literal,
        state: $state:ty,
        methods: [$($method:ident),* $(,)?],
        exec: $exec:path $(,)?
    ) => {
        static __PY_MOD_METHODS: &[$crate::ffi::PyMethodDef] = &[
            $($method::DEF,)*
            $crate::ffi::PyMethodDef::zeroed(),
        ];

        static __PY_MOD_ABI: $crate::ffi::PyABIInfo = $crate::module::abi_info();

        static __PY_MOD_SLOTS: $crate::module::SlotArray<12> = $crate::module::SlotArray([
            $crate::module::slot_static_data(
                $crate::ffi::Py_mod_name,
                concat!(stringify!($name), "\0").as_ptr() as *mut ::std::os::raw::c_void,
            ),
            $crate::module::slot_static_data(
                $crate::ffi::Py_mod_doc,
                $doc.as_ptr() as *mut ::std::os::raw::c_void,
            ),
            $crate::module::slot_size(
                $crate::ffi::Py_mod_state_size,
                $crate::module::state_size::<$state>(),
            ),
            $crate::module::slot_static_data(
                $crate::ffi::Py_mod_methods,
                __PY_MOD_METHODS.as_ptr() as *mut ::std::os::raw::c_void,
            ),
            $crate::module::slot_func($crate::module::PY_MOD_EXEC, {
                unsafe extern "C" fn __exec(m: *mut $crate::ffi::PyObject) -> ::std::os::raw::c_int {
                    unsafe { $crate::module::exec_impl::<$state>(m, $exec) }
                }
                // SAFETY: stored as the generic C slot function pointer type;
                // the interpreter calls it with the correct signature for
                // Py_mod_exec.
                Some(unsafe {
                    ::std::mem::transmute::<
                        unsafe extern "C" fn(*mut $crate::ffi::PyObject) -> ::std::os::raw::c_int,
                        unsafe extern "C" fn(),
                    >(__exec)
                })
            }),
            $crate::module::slot_func($crate::ffi::Py_mod_state_traverse, Some(unsafe {
                ::std::mem::transmute::<
                    unsafe extern "C" fn(
                        *mut $crate::ffi::PyObject,
                        $crate::ffi::visitproc,
                        *mut ::std::os::raw::c_void,
                    ) -> ::std::os::raw::c_int,
                    unsafe extern "C" fn(),
                >($crate::module::traverse_impl::<$state>)
            })),
            $crate::module::slot_func($crate::ffi::Py_mod_state_clear, Some(unsafe {
                ::std::mem::transmute::<
                    unsafe extern "C" fn(*mut $crate::ffi::PyObject) -> ::std::os::raw::c_int,
                    unsafe extern "C" fn(),
                >($crate::module::clear_impl::<$state>)
            })),
            $crate::module::slot_func($crate::ffi::Py_mod_state_free, Some(unsafe {
                ::std::mem::transmute::<
                    unsafe extern "C" fn(*mut ::std::os::raw::c_void),
                    unsafe extern "C" fn(),
                >($crate::module::free_impl::<$state>)
            })),
            $crate::module::slot_data(
                $crate::module::PY_MOD_MULTIPLE_INTERPRETERS,
                $crate::module::PY_MOD_PER_INTERPRETER_GIL_SUPPORTED,
            ),
            $crate::module::slot_data(
                $crate::module::PY_MOD_GIL,
                $crate::module::PY_MOD_GIL_NOT_USED,
            ),
            $crate::module::slot_static_data(
                $crate::ffi::Py_mod_abi,
                &raw const __PY_MOD_ABI as *mut ::std::os::raw::c_void,
            ),
            $crate::module::SLOT_END,
        ]);

        /// PEP 793 module export hook (exported as `PyModExport_<name>`).
        ///
        /// # Safety
        ///
        /// Called by the import machinery only.
        #[unsafe(export_name = concat!("PyModExport_", stringify!($name)))]
        pub unsafe extern "C" fn PyModExport() -> *mut $crate::ffi::PySlot {
            __PY_MOD_SLOTS.as_ptr()
        }
    };
}
