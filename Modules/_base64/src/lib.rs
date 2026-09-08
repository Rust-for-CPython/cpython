//! The `_base64` module, implemented against `cpython-api`.

use std::mem::MaybeUninit;

use cpython_api::prelude::*;

/// Per-module state, empty since the base64 module has no state
struct Base64State;

impl ModuleState for Base64State {
    fn new<'py>(_ts: &ThreadState<'py>, _module: &Bound<'py, PyModule>) -> PyResult<Self> {
        Ok(Base64State)
    }
}

const PAD_BYTE: u8 = b'=';
const ENCODE_TABLE: [u8; 64] = *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 4 output bytes per started block of 3 input bytes; `None` on overflow.
#[inline]
fn encoded_output_len(input_len: usize) -> Option<usize> {
    input_len.div_ceil(3).checked_mul(4)
}

/// Encode `input` into `output`, which must be exactly
/// `encoded_output_len(input.len())` bytes and is fully initialized on
/// return.
fn encode_into(input: &[u8], output: &mut [MaybeUninit<u8>]) {
    let mut chunks = input.chunks_exact(3);
    let mut out = output.iter_mut();
    let mut put = |b: u8| {
        out.next()
            .expect("output sized to encoded_output_len")
            .write(b);
    };

    for chunk in &mut chunks {
        let group = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        put(ENCODE_TABLE[(group >> 18 & 0x3f) as usize]);
        put(ENCODE_TABLE[(group >> 12 & 0x3f) as usize]);
        put(ENCODE_TABLE[(group >> 6 & 0x3f) as usize]);
        put(ENCODE_TABLE[(group & 0x3f) as usize]);
    }

    match *chunks.remainder() {
        [] => {}
        [a] => {
            let group = u32::from(a) << 16;
            put(ENCODE_TABLE[(group >> 18 & 0x3f) as usize]);
            put(ENCODE_TABLE[(group >> 12 & 0x3f) as usize]);
            put(PAD_BYTE);
            put(PAD_BYTE);
        }
        [a, b] => {
            let group = (u32::from(a) << 16) | (u32::from(b) << 8);
            put(ENCODE_TABLE[(group >> 18 & 0x3f) as usize]);
            put(ENCODE_TABLE[(group >> 12 & 0x3f) as usize]);
            put(ENCODE_TABLE[(group >> 6 & 0x3f) as usize]);
            put(PAD_BYTE);
        }
        _ => unreachable!("chunks_exact(3) remainder is at most 2 bytes"),
    }
}

/// Encode a bytes-like object with the standard Base64 alphabet.
#[pyfunction(signature = (data, /))]
fn standard_b64encode<'py>(
    ts: &ThreadState<'py>,
    _state: &Base64State,
    data: PyBuffer<'py>,
) -> PyResult<Bound<'py, PyBytes>> {
    let input = data.as_bytes();
    let Some(output_len) = encoded_output_len(input.len()) else {
        return Err(PyMemoryError::raise(ts, "encoded result too long"));
    };
    // Write straight into the bytes object's buffer — no intermediate Vec.
    PyBytes::new_with(ts, output_len, |output| {
        encode_into(input, output);
        Ok(())
    })
}

fn base64_exec<'py>(_ts: &ThreadState<'py>, _module: &Bound<'py, PyModule>) -> PyResult<()> {
    Ok(())
}

export_module! {
    name: _base64,
    doc: c"Base64 encoding implemented in Rust",
    state: Base64State,
    methods: [standard_b64encode],
    exec: base64_exec,
}
