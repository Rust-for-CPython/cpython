//! Owned Python object references: [`Bound`] (attached-scope) and [`Py`]
//! (`'static`, for storage in module state).

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::ffi;
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::PyAny;

/// An owned reference to a Python object, valid within the attached scope
/// `'py`.
///
/// `#[repr(transparent)]` over `NonNull<PyObject>`, so borrowed FFI pointers
/// (e.g. the vectorcall args array) can be lent as `&Bound` without any
/// refcount traffic.
///
/// `!Send + !Sync`: the reference is tied to this thread's attached scope. In
/// particular it can never enter a [`ThreadState::detach`] closure.
#[repr(transparent)]
pub struct Bound<'py, T> {
    ptr: NonNull<ffi::PyObject>,
    /// `&'py ()` ties the reference to the attached scope; `*mut ()` makes it
    /// `!Send + !Sync`; `fn() -> T` carries the type marker without affecting
    /// auto traits or drop-check.
    #[allow(clippy::type_complexity)]
    _marker: PhantomData<(&'py (), *mut (), fn() -> T)>,
}

/// An owned, `'static` reference to a Python object, for storage in module
/// state (and other places that outlive a single attached scope).
///
/// `Send + Sync`: the *handle* may move between threads; every operation on
/// it still requires `&ThreadState`. There is deliberately no `Clone` — use
/// [`Py::clone_ref`], which requires the token.
#[repr(transparent)]
pub struct Py<T> {
    ptr: NonNull<ffi::PyObject>,
    _marker: PhantomData<fn() -> T>,
}

// SAFETY: the handle is just a pointer; all operations (including clone)
// require `&ThreadState`. Drop is attachment-checked (see below).
unsafe impl<T> Send for Py<T> {}
unsafe impl<T> Sync for Py<T> {}

impl<'py, T> Bound<'py, T> {
    /// Take ownership of a strong reference returned by an FFI call.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid strong reference (this `Bound` will decref it on
    /// drop) to an object of type `T`.
    #[inline]
    pub unsafe fn from_owned_ptr(_ts: &ThreadState<'py>, ptr: NonNull<ffi::PyObject>) -> Self {
        Bound {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Lend a *borrowed* FFI pointer as a `&Bound` without touching the
    /// refcount.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid object of type `T` that stays alive (kept
    /// by its real owner) for as long as the returned reference is used.
    #[inline]
    pub unsafe fn ref_from_ptr<'a>(ptr: &'a NonNull<ffi::PyObject>) -> &'a Bound<'py, T> {
        // SAFETY: Bound is repr(transparent) over NonNull<PyObject>.
        unsafe { &*(ptr as *const NonNull<ffi::PyObject>).cast::<Bound<'py, T>>() }
    }

    /// The raw object pointer (borrowed; the `Bound` keeps its reference).
    #[inline]
    pub fn as_ptr(&self) -> *mut ffi::PyObject {
        self.ptr.as_ptr()
    }

    /// Create a new strong reference.
    #[inline]
    pub fn clone_ref(&self, ts: &ThreadState<'py>) -> Bound<'py, T> {
        unsafe {
            sys_calls::incref(ts, self.ptr.as_ptr());
            Bound::from_owned_ptr(ts, self.ptr)
        }
    }

    /// Detach from the `'py` scope, producing a `'static` handle.
    #[inline]
    pub fn unbind(self) -> Py<T> {
        let ptr = self.ptr;
        std::mem::forget(self);
        Py {
            ptr,
            _marker: PhantomData,
        }
    }

    /// View as a `Bound<PyAny>`.
    #[inline]
    pub fn as_any(&self) -> &Bound<'py, PyAny> {
        // SAFETY: identical repr(transparent) layout; PyAny is the base view.
        unsafe { &*(self as *const Bound<'py, T>).cast::<Bound<'py, PyAny>>() }
    }

    /// Convert into a `Bound<PyAny>`.
    #[inline]
    pub fn into_any(self) -> Bound<'py, PyAny> {
        let ptr = self.ptr;
        std::mem::forget(self);
        Bound {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Reinterpret as a different concrete type without checking.
    ///
    /// # Safety
    ///
    /// The object must actually be an instance of `U` (or a subtype).
    #[inline]
    pub unsafe fn cast_unchecked<U>(self) -> Bound<'py, U> {
        let ptr = self.ptr;
        std::mem::forget(self);
        Bound {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Hand the strong reference to C (e.g. as a trampoline return value).
    #[inline]
    pub fn into_ptr(self) -> *mut ffi::PyObject {
        let ptr = self.ptr;
        std::mem::forget(self);
        ptr.as_ptr()
    }

    /// Explicitly decref with the token, skipping the TLS attachment check
    /// that `Drop` performs.
    #[inline]
    pub fn drop_with(self, ts: &ThreadState<'py>) {
        let ptr = self.ptr;
        std::mem::forget(self);
        unsafe { sys_calls::decref(ts, ptr.as_ptr()) };
    }
}

impl<T> Py<T> {
    /// The raw object pointer (borrowed; the `Py` keeps its reference).
    #[inline]
    pub fn as_ptr(&self) -> *mut ffi::PyObject {
        self.ptr.as_ptr()
    }

    /// Borrow as a `&Bound` for the current attached scope, without touching
    /// the refcount.
    #[inline]
    pub fn bind<'a, 'py>(&'a self, _ts: &'a ThreadState<'py>) -> &'a Bound<'py, T> {
        // SAFETY: identical repr(transparent) layout; the borrow of self keeps
        // the reference alive, and the token borrow caps it to the attached
        // scope.
        unsafe { &*(self as *const Py<T>).cast::<Bound<'py, T>>() }
    }

    /// Convert into a `Bound`, re-entering the attached scope.
    #[inline]
    pub fn into_bound<'py>(self, _ts: &ThreadState<'py>) -> Bound<'py, T> {
        let ptr = self.ptr;
        std::mem::forget(self);
        Bound {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Create a new strong reference.
    #[inline]
    pub fn clone_ref(&self, ts: &ThreadState<'_>) -> Py<T> {
        unsafe { sys_calls::incref(ts, self.ptr.as_ptr()) };
        Py {
            ptr: self.ptr,
            _marker: PhantomData,
        }
    }

    /// Explicitly decref with the token, skipping the TLS attachment check
    /// that `Drop` performs.
    #[inline]
    pub fn drop_with(self, ts: &ThreadState<'_>) {
        let ptr = self.ptr;
        std::mem::forget(self);
        unsafe { sys_calls::decref(ts, ptr.as_ptr()) };
    }
}

/// Shared drop policy for `Bound` and `Py`: decref if this thread is
/// attached, otherwise leak (and fail a debug assertion).
///
/// Leaking is always sound; decref-ing while detached is not. In correct code
/// drops always happen attached — the detached case is only reachable by
/// deliberately smuggling a reference across a detach boundary or dropping a
/// `Py` on a non-Python thread, and turning those into leaks (never UB) is
/// the point of this check. See RUST_API.md §1.2.
#[inline]
fn drop_ref(ptr: NonNull<ffi::PyObject>) {
    unsafe {
        if !ffi::PyThreadState_GetUnchecked().is_null() {
            // tls-fallback: Drop has nowhere to store a token borrow; this is
            // the one deliberate TLS dependency (see RUST_API.md, explicit-
            // tstate policy). Use `drop_with` on hot paths.
            ffi::Py_DecRef(ptr.as_ptr());
        } else {
            debug_assert!(
                false,
                "Python object reference dropped while thread state is detached; leaking"
            );
        }
    }
}

impl<T> Drop for Bound<'_, T> {
    #[inline]
    fn drop(&mut self) {
        drop_ref(self.ptr);
    }
}

impl<T> Drop for Py<T> {
    #[inline]
    fn drop(&mut self) {
        drop_ref(self.ptr);
    }
}
