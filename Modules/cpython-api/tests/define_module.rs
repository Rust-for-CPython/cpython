//! Compile-and-shape test: defines a zlib-shaped module end-to-end.
//!
//! Nothing here starts an interpreter — the value of this test is that the
//! whole definition surface (proc macros, slot builders, module state,
//! class definition) compiles and const-evaluates, and that the resulting
//! static tables have the right shape.
//!
//! Gated on `has_libpython` (emitted by build.rs when the build tree
//! contains a static libpython): the test binary's static tables reference
//! interpreter symbols that the dynamic loader resolves at startup, so it
//! can only load when linked against a real libpython.
#![cfg(has_libpython)]

use std::sync::Mutex;

use cpython_api::class::{ClassDef, payload_offset};
use cpython_api::prelude::*;
use cpython_api::{ffi, module};

// --- module state -----------------------------------------------------------

struct TestState {
    error: Option<Py<PyType>>,
    compress_type: Option<Py<PyType>>,
}

impl ModuleState for TestState {
    fn new<'py>(ts: &ThreadState<'py>, m: &Bound<'py, PyModule>) -> PyResult<Self> {
        let error = PyType::new_exception(ts, c"testmod.error", None)?;
        m.add(ts, c"error", error.clone_ref(ts))?;
        let compress_type = COMPRESS_CLASS.create(ts, m)?;
        m.add_type(ts, &compress_type)?;
        Ok(TestState {
            error: Some(error.unbind()),
            compress_type: Some(compress_type.unbind()),
        })
    }

    fn traverse(&self, visit: &cpython_api::Visit<'_>) -> cpython_api::TraverseResult {
        if let Some(e) = &self.error {
            visit.call(e)?;
        }
        if let Some(t) = &self.compress_type {
            visit.call(t)?;
        }
        Ok(())
    }

    fn clear(&mut self) {
        self.error = None;
        self.compress_type = None;
    }
}

// --- module-level functions -------------------------------------------------

#[pyfunction(signature = (data, value = 1, /))]
fn adler32(
    _ts: &ThreadState<'_>,
    _state: &TestState,
    data: PyBuffer<'_>,
    value: u32,
) -> PyResult<u32> {
    Ok(value.wrapping_add(data.len() as u32))
}

// `ts: &mut ThreadState` opts in to `detach` (the trampoline passes the
// token through with the declared mutability).
#[pyfunction(signature = (data, /, level = -1, wbits = 15))]
fn compress<'py>(
    ts: &mut ThreadState<'py>,
    state: &TestState,
    data: PyBuffer<'py>,
    level: i32,
    wbits: i32,
) -> PyResult<Bound<'py, PyBytes>> {
    if !(-1..=9).contains(&level) || wbits == 0 {
        let error = state.error.as_ref().expect("state initialized");
        return Err(error.raise(ts, format_args!("Bad compression level {level}")));
    }
    // Long-running work runs with the thread state detached; the closure can
    // only capture Send data (the byte slice), never the token or a Bound.
    let input = data.as_bytes();
    let compressed = ts.detach(|| input.to_vec());
    PyBytes::new(ts, &compressed)
}

// A signature-less function: all parameters positional-or-keyword, required.
#[pyfunction]
fn crc32_combine(
    _ts: &ThreadState<'_>,
    _state: &TestState,
    crc1: u32,
    crc2: u32,
    len2: i64,
) -> PyResult<u32> {
    Ok(crc1 ^ crc2 ^ (len2 as u32))
}

// --- a class ---------------------------------------------------------------

struct Compress {
    inner: Mutex<Vec<u8>>,
}

#[pymethods]
impl Compress {
    #[pyfunction(signature = (data, /))]
    fn compress<'py>(
        &self,
        ts: &ThreadState<'py>,
        state: &TestState,
        data: PyBuffer<'py>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.len() > 1 << 20 {
            let error = state.error.as_ref().expect("state initialized");
            return Err(error.raise(ts, "buffer overflow"));
        }
        inner.extend_from_slice(data.as_bytes());
        PyBytes::new(ts, &inner)
    }

    #[pyfunction(signature = (mode = 4, /))]
    fn flush<'py>(
        &self,
        ts: &ThreadState<'py>,
        _state: &TestState,
        mode: i32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let _ = mode;
        PyBytes::new(ts, &self.inner.lock().unwrap())
    }

    #[getter]
    fn eof(&self, _ts: &ThreadState<'_>) -> bool {
        false
    }

    #[new]
    #[pyfunction(signature = (wbits = 15, zdict = None))]
    fn new(ts: &ThreadState<'_>, wbits: i32, zdict: Option<PyBuffer<'_>>) -> PyResult<Self> {
        if wbits == 0 {
            return Err(PyValueError::raise(ts, "invalid wbits"));
        }
        Ok(Compress {
            inner: Mutex::new(zdict.map(|z| z.as_bytes().to_vec()).unwrap_or_default()),
        })
    }
}

static COMPRESS_CLASS: ClassDef<Compress> = ClassDef::new(c"testmod._Compress")
    .methods(&COMPRESS_METHODS)
    .getsets(&COMPRESS_GETSETS)
    .tp_new(COMPRESS_TP_NEW);

// --- factory function using new_instance ------------------------------------

#[pyfunction(signature = (level = -1))]
fn compressobj<'py>(
    ts: &ThreadState<'py>,
    state: &TestState,
    level: i32,
) -> PyResult<Bound<'py, PyAny>> {
    let _ = level;
    let cls = state.compress_type.as_ref().expect("state initialized");
    cpython_api::class::new_instance(
        ts,
        cls.bind(ts),
        Compress {
            inner: Mutex::new(Vec::new()),
        },
    )
}

// --- the module -------------------------------------------------------------

fn testmod_exec<'py>(ts: &ThreadState<'py>, m: &Bound<'py, PyModule>) -> PyResult<()> {
    m.add(ts, c"MAX_WBITS", 15)?;
    m.add(ts, c"DEF_BUF_SIZE", 16384usize)?;
    m.add(ts, c"ZLIB_VERSION", "1.3.1")?;
    Ok(())
}

export_module! {
    name: testmod,
    doc: c"A zlib-shaped test module",
    state: TestState,
    methods: [adler32, compress, crc32_combine, compressobj],
    exec: testmod_exec,
}

// --- shape checks -----------------------------------------------------------

#[test]
fn method_table_shape() {
    // 4 methods + terminator, built by export_module!.
    let def = &adler32::DEF;
    assert_eq!(
        unsafe { std::ffi::CStr::from_ptr(def.ml_name) }
            .to_str()
            .unwrap(),
        "adler32"
    );
    assert_eq!(def.ml_flags, ffi::METH_FASTCALL | ffi::METH_KEYWORDS);

    // Module functions stay FASTCALL (state comes from the module object);
    // methods are uniformly METH_METHOD (state via defining class).
    assert_eq!(
        compress::DEF.ml_flags,
        ffi::METH_FASTCALL | ffi::METH_KEYWORDS
    );
    for def in &COMPRESS_METHODS.0[..2] {
        assert_eq!(
            def.ml_flags,
            ffi::METH_FASTCALL | ffi::METH_KEYWORDS | ffi::METH_METHOD
        );
    }
    // Terminator.
    assert!(COMPRESS_METHODS.0[2].ml_name.is_null());
}

#[test]
fn spec_shape() {
    assert_eq!(adler32::SPEC.fn_name, "adler32");
    assert_eq!(adler32::SPEC.params.len(), 2);
    assert_eq!(adler32::SPEC.pos_only, 2);
    assert!(adler32::SPEC.params[0].required);
    assert!(!adler32::SPEC.params[1].required);

    assert_eq!(compress::SPEC.pos_only, 1);
    assert_eq!(compress::SPEC.params.len(), 3);

    // Signature-less: all positional-or-keyword, required.
    assert_eq!(crc32_combine::SPEC.pos_only, 0);
    assert_eq!(crc32_combine::SPEC.params.len(), 3);
    assert!(crc32_combine::SPEC.params.iter().all(|p| p.required));
}

#[test]
fn getset_table_shape() {
    assert_eq!(
        unsafe { std::ffi::CStr::from_ptr(COMPRESS_GETSETS.0[0].name) }
            .to_str()
            .unwrap(),
        "eof"
    );
    assert!(COMPRESS_GETSETS.0[0].get.is_some());
    assert!(COMPRESS_GETSETS.0[0].set.is_none());
    assert!(COMPRESS_GETSETS.0[1].name.is_null());
}

#[test]
fn payload_layout() {
    // Payload starts after the header, aligned for the payload type.
    let off = payload_offset::<Compress>();
    assert!(off >= std::mem::size_of::<ffi::PyObject>());
    assert_eq!(off % std::mem::align_of::<Compress>(), 0);
}

#[test]
fn abi_info_shape() {
    let abi = module::abi_info();
    assert_eq!(abi.abiinfo_major_version, 1);
    assert_ne!(abi.flags & ffi::PyABIInfo_INTERNAL as u16, 0);
    // Exactly one of GIL / FREETHREADED, matching the interpreter build.
    let ft = abi.flags & (ffi::PyABIInfo_GIL | ffi::PyABIInfo_FREETHREADED) as u16;
    assert!(ft == ffi::PyABIInfo_GIL as u16 || ft == ffi::PyABIInfo_FREETHREADED as u16);
    assert_eq!(abi.build_version, module::PY_VERSION_HEX);
}

#[test]
fn module_slots_shape() {
    // The export hook returns the static slot array; walk it and check the
    // required PEP 793 slots are present, terminated, and defaulted to
    // free-threading + per-interpreter-GIL support.
    let slots = unsafe { PyModExport() };
    let mut ids = Vec::new();
    let mut i = 0;
    loop {
        let slot = unsafe { *slots.add(i) };
        if slot.sl_id == 0 {
            break;
        }
        ids.push(slot.sl_id as u32);
        i += 1;
        assert!(i < 32, "unterminated slot array");
    }
    for required in [
        ffi::Py_mod_name,
        ffi::Py_mod_doc,
        ffi::Py_mod_state_size,
        ffi::Py_mod_methods,
        module::PY_MOD_EXEC,
        ffi::Py_mod_state_traverse,
        ffi::Py_mod_state_clear,
        ffi::Py_mod_state_free,
        module::PY_MOD_MULTIPLE_INTERPRETERS,
        module::PY_MOD_GIL,
        ffi::Py_mod_abi,
    ] {
        assert!(ids.contains(&required), "missing slot id {required}");
    }
}
