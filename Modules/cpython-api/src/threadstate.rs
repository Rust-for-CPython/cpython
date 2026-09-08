//! The explicit thread-state token.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::ffi;

/// Proof of an attached thread state, wrapping the actual `PyThreadState*`.
///
/// Every operation that touches the interpreter takes `&ThreadState<'py>`.
/// The wrapped pointer lets the crate call explicit-tstate C APIs directly
/// instead of re-fetching the thread state from TLS.
///
/// Soundness properties:
///
/// - Not `Copy`/`Clone`: [`ThreadState::detach`] takes `&mut self`, so the
///   exclusive borrow statically excludes every other use of the token while
///   the thread state is detached — including uses smuggled through `Send`
///   wrappers (any `&ThreadState`, however wrapped, is still a borrow).
/// - `!Send`/`!Sync` (via the `*mut ()` marker): the token can neither move to
///   nor be shared with another thread, and a detach closure (which must be
///   `Send`) can never capture one.
/// - The lifetime `'py` is invariant, so tokens with different attachment
///   scopes never unify.
#[repr(transparent)]
pub struct ThreadState<'py> {
    ptr: NonNull<ffi::PyThreadState>,
    /// `fn(&'py ()) -> &'py ()` makes `'py` invariant; `*mut ()` makes the
    /// type `!Send + !Sync`.
    #[allow(clippy::type_complexity)]
    _marker: PhantomData<(fn(&'py ()) -> &'py (), *mut ())>,
}

impl<'py> ThreadState<'py> {
    /// Wrap a raw thread-state pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be the thread state attached to the *current* thread, and it
    /// must remain attached for the whole lifetime `'py` (except inside
    /// [`ThreadState::detach`], which re-establishes attachment before
    /// returning).
    #[inline]
    pub unsafe fn from_raw(ptr: NonNull<ffi::PyThreadState>) -> ThreadState<'py> {
        ThreadState {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Fetch the current thread's attached thread state from TLS.
    ///
    /// This is the entry-point shim used by generated trampolines: extension
    /// entry points are only ever invoked with an attached thread state.
    ///
    /// # Safety
    ///
    /// The current thread must have an attached thread state (true whenever
    /// Python calls into extension code), and it must stay attached for `'py`.
    #[doc(hidden)]
    #[inline]
    pub unsafe fn current_unchecked() -> ThreadState<'py> {
        // tls-fallback by nature: this is the one place a trampoline has to
        // consult TLS, converting the implicit attachment into the explicit
        // token everything else uses.
        let ptr = unsafe { ffi::PyThreadState_GetUnchecked() };
        debug_assert!(!ptr.is_null(), "no attached thread state at entry point");
        unsafe { ThreadState::from_raw(NonNull::new_unchecked(ptr)) }
    }

    /// The raw `PyThreadState*`.
    #[inline]
    pub fn as_ptr(&self) -> *mut ffi::PyThreadState {
        self.ptr.as_ptr()
    }

    /// True if an exception is currently set on this thread.
    ///
    /// Reads the thread state's `current_exception` field directly
    /// (explicit-tstate; no TLS fetch).
    #[inline]
    pub fn exception_set(&self) -> bool {
        unsafe { !(*self.ptr.as_ptr()).current_exception.is_null() }
    }

    /// Detach the thread state (releasing the GIL on GIL builds, and allowing
    /// stop-the-world pauses on free-threaded builds), run `f`, then reattach.
    ///
    /// The closure runs without any access to the interpreter:
    ///
    /// - it has no token, and `F: Send` means it cannot capture
    ///   `&ThreadState` (`!Sync`) or any `Bound` reference (`!Send`);
    /// - the `&mut self` borrow prevents any other use of this token for the
    ///   duration, even through `Send`-laundering wrappers.
    pub fn detach<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R + Send,
        R: Send,
    {
        struct ReattachGuard(*mut ffi::PyThreadState);
        impl Drop for ReattachGuard {
            fn drop(&mut self) {
                // Reattach even if `f` panics, so unwinding back into
                // interpreter-touching code is sound.
                unsafe { ffi::PyEval_RestoreThread(self.0) };
            }
        }

        let saved = unsafe { ffi::PyEval_SaveThread() };
        debug_assert_eq!(
            saved,
            self.ptr.as_ptr(),
            "detached a different thread state than the token wraps"
        );
        let _guard = ReattachGuard(saved);
        f()
    }
}
