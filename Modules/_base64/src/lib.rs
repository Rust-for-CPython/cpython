use std::cell::UnsafeCell;
use std::ffi::{c_char, c_int, c_void};
use std::mem::MaybeUninit;
use std::ptr;
use std::slice;

use cpython_sys::METH_FASTCALL;
use cpython_sys::PyBytes_AsString;
use cpython_sys::PyBytes_FromStringAndSize;
use cpython_sys::PyBuffer_Release;
use cpython_sys::PyMethodDef;
use cpython_sys::PyMethodDefFuncPointer;
use cpython_sys::PyModuleDef;
use cpython_sys::PyModuleDef_HEAD_INIT;
use cpython_sys::PyModuleDef_Init;
use cpython_sys::PyObject;
use cpython_sys::PyObject_GetBuffer;
use cpython_sys::Py_DecRef;
use cpython_sys::PyErr_NoMemory;
use cpython_sys::PyErr_SetString;
use cpython_sys::PyExc_TypeError;
use cpython_sys::Py_buffer;
use cpython_sys::Py_ssize_t;

const PYBUF_SIMPLE: c_int = 0;
const PAD_BYTE: u8 = b'=';
const ENCODE_TABLE: [u8; 64] =
    *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[inline]
fn encoded_output_len(input_len: usize) -> Option<usize> {
    input_len
        .checked_add(2)
        .map(|n| n / 3)
        .and_then(|blocks| blocks.checked_mul(4))
}

#[inline]
fn encode_into(input: &[u8], output: &mut [u8]) -> usize {
    let mut src_index = 0;
    let mut dst_index = 0;
    let len = input.len();

    while src_index + 3 <= len {
        let chunk = (u32::from(input[src_index]) << 16)
            | (u32::from(input[src_index + 1]) << 8)
            | u32::from(input[src_index + 2]);
        output[dst_index] = ENCODE_TABLE[((chunk >> 18) & 0x3f) as usize];
        output[dst_index + 1] = ENCODE_TABLE[((chunk >> 12) & 0x3f) as usize];
        output[dst_index + 2] = ENCODE_TABLE[((chunk >> 6) & 0x3f) as usize];
        output[dst_index + 3] = ENCODE_TABLE[(chunk & 0x3f) as usize];
        src_index += 3;
        dst_index += 4;
    }

    match len - src_index {
        0 => {}
        1 => {
            let chunk = u32::from(input[src_index]) << 16;
            output[dst_index] = ENCODE_TABLE[((chunk >> 18) & 0x3f) as usize];
            output[dst_index + 1] = ENCODE_TABLE[((chunk >> 12) & 0x3f) as usize];
            output[dst_index + 2] = PAD_BYTE;
            output[dst_index + 3] = PAD_BYTE;
            dst_index += 4;
        }
        2 => {
            let chunk = (u32::from(input[src_index]) << 16)
                | (u32::from(input[src_index + 1]) << 8);
            output[dst_index] = ENCODE_TABLE[((chunk >> 18) & 0x3f) as usize];
            output[dst_index + 1] = ENCODE_TABLE[((chunk >> 12) & 0x3f) as usize];
            output[dst_index + 2] = ENCODE_TABLE[((chunk >> 6) & 0x3f) as usize];
            output[dst_index + 3] = PAD_BYTE;
            dst_index += 4;
        }
        _ => unreachable!("len - src_index cannot exceed 2"),
    }

    dst_index
}

struct BorrowedBuffer {
    view: Py_buffer,
}

impl BorrowedBuffer {
    unsafe fn from_object(obj: *mut PyObject) -> Result<Self, ()> {
        let mut view = MaybeUninit::<Py_buffer>::uninit();
        if unsafe { PyObject_GetBuffer(obj, view.as_mut_ptr(), PYBUF_SIMPLE) } != 0 {
            return Err(());
        }
        Ok(Self {
            view: unsafe { view.assume_init() },
        })
    }

    fn len(&self) -> Py_ssize_t {
        self.view.len
    }

    fn as_ptr(&self) -> *const u8 {
        self.view.buf.cast::<u8>() as *const u8
    }
}

impl Drop for BorrowedBuffer {
    fn drop(&mut self) {
        unsafe {
            PyBuffer_Release(&mut self.view);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn standard_b64encode(
    _module: *mut PyObject,
    args: *mut *mut PyObject,
    nargs: Py_ssize_t,
) -> *mut PyObject {
    if nargs != 1 {
        unsafe {
            PyErr_SetString(
                PyExc_TypeError,
                c"standard_b64encode() takes exactly one argument".as_ptr(),
            );
        }
        return ptr::null_mut();
    }

    let source = unsafe { *args };
    let buffer = match unsafe { BorrowedBuffer::from_object(source) } {
        Ok(buf) => buf,
        Err(_) => return ptr::null_mut(),
    };

    let view_len = buffer.len();
    if view_len < 0 {
        unsafe {
            PyErr_SetString(
                PyExc_TypeError,
                c"standard_b64encode() argument has negative length".as_ptr(),
            );
        }
        return ptr::null_mut();
    }
    let input_len = view_len as usize;
    let input = unsafe { slice::from_raw_parts(buffer.as_ptr(), input_len) };

    let Some(output_len) = encoded_output_len(input_len) else {
        unsafe {
            PyErr_NoMemory();
        }
        return ptr::null_mut();
    };

    if output_len > isize::MAX as usize {
        unsafe {
            PyErr_NoMemory();
        }
        return ptr::null_mut();
    }

    let result = unsafe {
        PyBytes_FromStringAndSize(ptr::null(), output_len as Py_ssize_t)
    };
    if result.is_null() {
        return ptr::null_mut();
    }

    let dest_ptr = unsafe { PyBytes_AsString(result) };
    if dest_ptr.is_null() {
        unsafe {
            Py_DecRef(result);
        }
        return ptr::null_mut();
    }
    let dest = unsafe { slice::from_raw_parts_mut(dest_ptr.cast::<u8>(), output_len) };

    let written = encode_into(input, dest);
    debug_assert_eq!(written, output_len);
    result
}

#[unsafe(no_mangle)]
pub extern "C" fn _base64_clear(_obj: *mut PyObject) -> c_int {
    //TODO
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn _base64_free(_o: *mut c_void) {
    //TODO
}

pub struct ModuleDef {
    ffi: UnsafeCell<PyModuleDef>,
}

impl ModuleDef {
    fn init_multi_phase(&'static self) -> *mut PyObject {
        unsafe { PyModuleDef_Init(self.ffi.get()) }
    }
}

unsafe impl Sync for ModuleDef {}

pub static _BASE64_MODULE_METHODS: [PyMethodDef; 2] = {
    [
        PyMethodDef {
            ml_name: c"standard_b64encode".as_ptr() as *mut c_char,
            ml_meth: PyMethodDefFuncPointer {
                PyCFunctionFast: standard_b64encode,
            },
            ml_flags: METH_FASTCALL,
            ml_doc: c"Demo for the _base64 module".as_ptr() as *mut c_char,
        },
        PyMethodDef::zeroed(),
    ]
};

pub static _BASE64_MODULE: ModuleDef = {
    ModuleDef {
        ffi: UnsafeCell::new(PyModuleDef {
            m_base: PyModuleDef_HEAD_INIT,
            m_name: c"_base64".as_ptr() as *mut _,
            m_doc: c"A test Rust module".as_ptr() as *mut _,
            m_size: 0,
            m_methods: &_BASE64_MODULE_METHODS as *const PyMethodDef as *mut _,
            m_slots: std::ptr::null_mut(),
            m_traverse: None,
            m_clear: Some(_base64_clear),
            m_free: Some(_base64_free),
        }),
    }
};

#[unsafe(no_mangle)]
pub extern "C" fn PyInit__base64() -> *mut PyObject {
    _BASE64_MODULE.init_multi_phase()
}
