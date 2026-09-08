//! Runtime argument binding for generated trampolines.
//!
//! The `#[pyfunction]` macro emits a static [`ParamSpec`] describing the
//! Python-visible signature and calls [`parse_fastcall`] (vectorcall
//! functions/methods) or [`parse_tuple_dict`] (`tp_new`) to bind incoming
//! arguments to parameter slots. Extraction to Rust types then happens in
//! generated code via `FromPyObject`.

use std::ffi::CStr;
use std::ptr::NonNull;

use crate::err::{PyErrRaised, PyResult, PyTypeError};
use crate::ffi;
use crate::instance::Bound;
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::PyAny;

/// One Python-visible parameter.
pub struct Param {
    /// Keyword name (also used in error messages).
    pub name: &'static CStr,
    /// False if the parameter has a default in the generated code.
    pub required: bool,
}

/// The Python-visible signature of a function.
pub struct ParamSpec {
    /// Function name for error messages (e.g. `"compress"`).
    pub fn_name: &'static str,
    pub params: &'static [Param],
    /// The first `pos_only` parameters are positional-only (`/` marker).
    pub pos_only: usize,
}

/// Bound argument slots: a borrowed object pointer per parameter, `None`
/// where the default applies.
pub type ArgSlots<const N: usize> = [Option<NonNull<ffi::PyObject>>; N];

/// View a bound slot as a borrowed `&Bound<PyAny>`.
#[inline]
pub fn arg_bound<'py, const N: usize>(
    slots: &ArgSlots<N>,
    index: usize,
) -> Option<&Bound<'py, PyAny>> {
    slots[index]
        .as_ref()
        .map(|p| unsafe { Bound::ref_from_ptr(p) })
}

fn raise_type_error(ts: &ThreadState<'_>, msg: std::fmt::Arguments<'_>) -> PyErrRaised {
    PyTypeError::raise(ts, msg)
}

/// Bind a vectorcall argument array (`METH_FASTCALL | METH_KEYWORDS`) to
/// parameter slots.
///
/// # Safety
///
/// `args`/`nargs`/`kwnames` must be exactly what the interpreter passed to a
/// fastcall entry point; the returned pointers are borrowed from that array
/// and valid for the duration of the call.
pub unsafe fn parse_fastcall<'py, const N: usize>(
    ts: &ThreadState<'py>,
    spec: &ParamSpec,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<ArgSlots<N>> {
    debug_assert_eq!(spec.params.len(), N);
    let mut slots: ArgSlots<N> = [None; N];

    let nargs = nargs as usize;
    if nargs > N {
        return Err(raise_type_error(
            ts,
            format_args!(
                "{}() takes at most {} argument{} ({} given)",
                spec.fn_name,
                N,
                if N == 1 { "" } else { "s" },
                nargs
            ),
        ));
    }
    for (i, slot) in slots.iter_mut().enumerate().take(nargs) {
        // SAFETY: the interpreter guarantees `nargs` valid entries.
        *slot = NonNull::new(unsafe { *args.add(i) });
        debug_assert!(slot.is_some());
    }

    if !kwnames.is_null() {
        let nkw = unsafe { sys_calls::tuple_size(ts, kwnames) };
        for k in 0..nkw {
            let name = unsafe { sys_calls::tuple_get_item(ts, kwnames, k) };
            let value = NonNull::new(unsafe { *args.add(nargs + k as usize) });
            let mut matched = false;
            for (i, param) in spec.params.iter().enumerate().skip(spec.pos_only) {
                if unsafe { sys_calls::unicode_eq_ascii(name, param.name.as_ptr()) } != 0 {
                    if slots[i].is_some() {
                        return Err(raise_type_error(
                            ts,
                            format_args!(
                                "argument for {}() given by name ('{}') and position ({})",
                                spec.fn_name,
                                param.name.to_str().unwrap_or("?"),
                                i + 1,
                            ),
                        ));
                    }
                    slots[i] = value;
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Err(raise_type_error(
                    ts,
                    format_args!("{}() got an unexpected keyword argument", spec.fn_name),
                ));
            }
        }
    }

    check_required(ts, spec, &slots)?;
    Ok(slots)
}

/// Bind a classic `(args_tuple, kwargs_dict)` pair (`tp_new`) to parameter
/// slots.
///
/// The returned pointers are borrowed: positional values are borrowed from
/// the tuple (alive for the call), keyword values are borrowed from the
/// call-private kwargs dict.
///
/// # Safety
///
/// `args` must be the argument tuple and `kwargs` the (possibly null) keyword
/// dict the interpreter passed to a `tp_new` entry point.
pub unsafe fn parse_tuple_dict<'py, const N: usize>(
    ts: &ThreadState<'py>,
    spec: &ParamSpec,
    args: *mut ffi::PyObject,
    kwargs: *mut ffi::PyObject,
) -> PyResult<ArgSlots<N>> {
    debug_assert_eq!(spec.params.len(), N);
    let mut slots: ArgSlots<N> = [None; N];

    let nargs = unsafe { sys_calls::tuple_size(ts, args) } as usize;
    if nargs > N {
        return Err(raise_type_error(
            ts,
            format_args!(
                "{}() takes at most {} argument{} ({} given)",
                spec.fn_name,
                N,
                if N == 1 { "" } else { "s" },
                nargs
            ),
        ));
    }
    for (i, slot) in slots.iter_mut().enumerate().take(nargs) {
        let item = unsafe { sys_calls::tuple_get_item(ts, args, i as ffi::Py_ssize_t) };
        if item.is_null() {
            return Err(unsafe { PyErrRaised::assume_set(ts) });
        }
        *slot = NonNull::new(item);
    }

    if !kwargs.is_null() {
        let total = unsafe { sys_calls::dict_size(ts, kwargs) };
        let mut matched: ffi::Py_ssize_t = 0;
        for (i, param) in spec.params.iter().enumerate().skip(spec.pos_only) {
            let mut value: *mut ffi::PyObject = std::ptr::null_mut();
            let found = unsafe {
                sys_calls::dict_get_item_string_ref(ts, kwargs, param.name.as_ptr(), &mut value)
            };
            match found {
                -1 => return Err(unsafe { PyErrRaised::assume_set(ts) }),
                0 => {}
                _ => {
                    // Downgrade the strong reference to a borrow: the kwargs
                    // dict is private to this call and keeps the value alive.
                    unsafe { sys_calls::decref(ts, value) };
                    if slots[i].is_some() {
                        return Err(raise_type_error(
                            ts,
                            format_args!(
                                "argument for {}() given by name ('{}') and position ({})",
                                spec.fn_name,
                                param.name.to_str().unwrap_or("?"),
                                i + 1,
                            ),
                        ));
                    }
                    slots[i] = NonNull::new(value);
                    matched += 1;
                }
            }
        }
        if matched != total {
            return Err(raise_type_error(
                ts,
                format_args!("{}() got an unexpected keyword argument", spec.fn_name),
            ));
        }
    }

    check_required(ts, spec, &slots)?;
    Ok(slots)
}

fn check_required<const N: usize>(
    ts: &ThreadState<'_>,
    spec: &ParamSpec,
    slots: &ArgSlots<N>,
) -> PyResult<()> {
    for (i, param) in spec.params.iter().enumerate() {
        if param.required && slots[i].is_none() {
            return Err(raise_type_error(
                ts,
                format_args!(
                    "{}() missing required argument '{}' (pos {})",
                    spec.fn_name,
                    param.name.to_str().unwrap_or("?"),
                    i + 1,
                ),
            ));
        }
    }
    Ok(())
}
