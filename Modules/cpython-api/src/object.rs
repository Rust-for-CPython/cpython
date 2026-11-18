use std::cell::UnsafeCell;

use cpython_sys::PyObject;

#[repr(transparent)]
pub struct PyAny(UnsafeCell<PyObject>);

#[cfg(test)]
mod tests {
    use std::mem::{align_of, size_of};

    use super::*;

    #[test]
    fn layout_matches_pyobject() {
        assert_eq!(size_of::<PyAny>(), size_of::<PyObject>());
        assert_eq!(align_of::<PyAny>(), align_of::<PyObject>());
    }

    #[test]
    fn pointer_layout_matches_pyobject() {
        assert_eq!(size_of::<*mut PyAny>(), size_of::<*mut PyObject>());
    }
}
