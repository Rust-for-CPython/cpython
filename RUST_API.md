# Rust for CPython API Design Doc for 3.16

### Overview

This document designs a safe Rust API for in-tree CPython modules, initially
covering the features needed by `zlib` and providing a base for later Rust
work.

### Context

Rust can call CPython's C API efficiently, but raw FFI does not encode Rust's
lifetime, aliasing, or thread-safety rules. A safe wrapper is needed to retain
those guarantees when implementing CPython modules in Rust.

### Goals

In priority order, the API should:

1. be sound on GIL and free-threaded builds;
2. support every PEP 11 platform;
3. approach equivalent C performance where safety permits;
4. feel familiar to PyO3 and C API users; and
5. use modern APIs such as `PyModExport`.

Every use of `unsafe` requires a `// SAFETY:` comment stating its proof
obligation. No panic may escape a non-unwinding C ABI.

### Non-goals

Full C API parity and subinterpreter support are out of scope. Wrappers are
added as modules need them; generated modules reject subinterpreter imports.

### Specific guidelines

- Encode CPython contracts in the type system where practical.
- Use macros sparingly to control compile time.
- Preserve Rust's initialization, aliasing, and concurrency invariants.
- Support both GIL and free-threaded builds.

### API design

1. Bindings and the FFI boundary
2. Managing the thread context
3. Python object handles and garbage collection
4. Exceptions and error handling
5. Concrete object abstractions
6. Rust/Python conversions
7. Module definition
8. Function definition
9. Class and method definition
10. Argument parsing
11. Testing
12. Open decisions and future work

#### Bindings and the FFI boundary

`cpython-sys` generates raw bindings at build time from `Python.h`, internal
headers, and the active `pyconfig.h`. This captures platform, target, debug,
and free-threading-dependent layout information. Bindgen allowlists public and private
`Py*` and `_Py*` names. Some C constructs are not replicatable via C bindings such as macros.
These constructs are replicated in Rust.

Python objects allow mutation through shared pointers, so their binding uses
`UnsafeCell`:

```rust
#[repr(transparent)]
pub struct PyObject(UnsafeCell<_object>);
```

All C calls pass through a private `c_api` module. A safe operation that may
touch Python requires `ThreadContext`, an abstraction over the thread-state; an operation without one needs an
audited proof that its entire call path is thread-state-free. Existing C APIs
may obtain the current state internally.

#### Managing the thread context

`ThreadContext<'py>` proves that the current OS thread has an attached CPython
thread state for `'py`:

```rust
pub struct ThreadContext<'py> {
    ptr: NonNull<ffi::PyThreadState>,
    detach: Option<DetachPermit<'py>>,
    #[allow(clippy::type_complexity)]
    _marker: PhantomData<(fn(&'py ()) -> &'py (), *mut ())>,
}

struct DetachPermit<'py> {
    _marker: PhantomData<&'py mut RustEntryGuard>,
}
```

The invariant marker makes the context `!Send + !Sync`. Neither type is
`Copy` or `Clone`, and only generated entry glue constructs them.

Each FFI entry increments a depth stored on the actual `PyThreadState` and
holds an unwind-safe `RustEntryGuard` for the full callback. Process-wide
storage is required because separate `cdylib`s may contain separate Rust TLS.
Only depth one receives a `DetachPermit`; reentrant callbacks remain attached
but cannot detach. `RustEntryGuard::drop` restores the exact previous depth on
normal return and unwind.

Entry uses a higher-ranked closure so callers cannot choose `'py = 'static` or
return an attached value:

```rust
impl RustEntryGuard {
    fn context<'scope>(&'scope mut self) -> ThreadContext<'scope> {
        let detach = self.is_outermost().then(|| DetachPermit {
            _marker: PhantomData,
        });
        ThreadContext {
            ptr: self.ptr(),
            detach,
            _marker: PhantomData,
        }
    }
}

// SAFETY: called only from generated C callbacks with an attached state.
unsafe fn with_current<R>(
    f: impl for<'py> FnOnce(&mut ThreadContext<'py>) -> R,
) -> R {
    // SAFETY: guaranteed by the caller.
    let ptr = unsafe { current_attached_tstate() };
    let mut entry = RustEntryGuard::enter(ptr);
    let mut ctx = entry.context();
    f(&mut ctx)
}
```

The guard borrow keeps it alive through the `'py` lifetime; `for<'py>` makes the lifetime
fresh; and `R` cannot depend on it. Generated wrappers convert attached results to
raw C values before returning. Macros reject author functions that request a
specific lifetime such as `'static`.

Callback arguments have a separate owner lifetime, described under argument
parsing; constructing that owner is unsafe and confined to generated glue code.

Detachment requires `&mut ThreadContext` and the outermost-entry permit:

```rust
impl<'py> ThreadContext<'py> {
    pub fn detach<F, R>(&mut self, f: F) -> PyResult<R>
    where
        F: FnOnce() -> R + Send,
        R: Send,
    {
        self.detach_impl(f)
    }
}
```

The exclusive borrow prevents use of that context while detached. The `Send`
bounds exclude attached handles from the closure and its result. Missing
permits produce `RuntimeError` without detaching. Internally, a guard uses
`_PyThreadState_Detach` and `_PyThreadState_Attach` to restore the exact state
even if `f` panics:

```rust
let output = ctx.detach(move || compress_without_python(input))?;
```

#### Python object handles and garbage collection

Three private generic handle representations distinguish borrowing, ownership,
and attachment:

| Handle | Owns a reference | Lifetime | Auto traits | Drop |
| --- | --- | --- | --- | --- |
| `Borrowed<'a, 'py, T>` | No | owner `'a` and attachment `'py` | `Copy`, `!Send`, `!Sync` | none |
| `Bound<'py, T>` | Yes | attachment `'py` | `!Send`, `!Sync` | decref while attached |
| `Py<T>` | Yes | unbound, for stored state | `Send`, `Sync` | checked policy below |

The two `Borrowed` lifetimes are independent: an owner keeps the object alive
for `'a`, while `ThreadContext` permits access for `'py`. `Py<T>` may cross
threads, but touching its object still requires an active context.

Moving among the forms is explicit:

```rust
let stored: Py<PyType> = exception_type.unbind();
let current: Borrowed<'_, 'py, PyType> = stored.bind(ctx);
let retained: Bound<'py, PyType> = current.to_owned(ctx);
```

Neither owning handle implements `Clone`; `clone_ref(ctx)` and
`to_owned(ctx)` make each incref explicit. Raw borrowed and owned-pointer
constructors are private. Their glue proves non-nullness, interpreter identity,
liveness, and the C ownership contract before constructing a handle.

Owning handles implement consuming explicit destruction:

```rust
object.drop_with(ctx);
```

`drop_with` uses `ManuallyDrop` to avoid a second automatic drop. Ordinary
`Drop` cannot receive a context, so it checks attachment with
`PyThreadState_GetUnchecked`: attached drops decref; detached drops leak and
record a Python-free, non-reentrant, non-allocating, non-panicking diagnostic.
Leaking is a programming error but remains memory-safe. `Bound` normally
cannot reach this case because it is attachment-bound and `!Send + !Sync`.

`PyAny` is the erased object marker. More specific markers, such as `PyBytes`
and `PyType`, have the same transparent layout, so upcasts to `PyAny` do not
touch the refcount. There is intentionally no `&mut PyAny`: Python objects
have interior mutability and can be changed through aliases, especially in a
free-threaded build.

Cyclic GC tracking is handled for modules and classes and described in their respective sections.


#### Exceptions and error handling

Errors use a non-owning marker for the current thread's pending exception:

```rust
#[must_use]
pub struct PyErrRaised {
    _priv: PhantomData<*mut ()>,
}

pub type PyResult<T> = Result<T, PyErrRaised>;
```

`PyErrRaised` is `!Send` and `#[must_use]`. Built-in and custom exception types
provide `raise(ctx, message)`, so Rust code can use `?` without copying the
Python exception:

```rust
fn checked_size(ctx: &ThreadContext<'_>, size: usize) -> PyResult<i32> {
    i32::try_from(size)
        .map_err(|_| PyOverflowError::raise(ctx, "size is too large"))
}
```

C wrappers apply each function's error contract. Ambiguous numeric sentinels
also require checking whether an exception is pending. Once failure is known,
`assume_set` converts the ambient exception to the marker:

```rust
if ptr.is_null() {
    // SAFETY: this branch is reached only for a C API whose documented NULL
    // result guarantees that it set an exception on `ctx`'s thread state.
    Err(unsafe { PyErrRaised::assume_set(ctx) })
} else {
    // Turn the owned pointer into a Bound value.
}
```

`assume_set` is unsafe because a false marker would let a callback return an
error sentinel without an exception; it therefore debug-checks the state. At
the FFI boundary, `Err` requires a pending exception; otherwise the trampoline
sets `SystemError`. `Ok` requires no pending exception; on mismatch the
trampoline releases the result and returns its error sentinel. These checks
also reject stale saved markers.


#### Concrete object abstractions

Concrete types such as `PyBytes` and `PyModule` are transparent markers. APIs
operate on the marker or `Bound<'py, Marker>`, so type-specific methods require
a checked or statically known marker. The initial set grows with module needs.

`PyBytes::new_with` zeroes an unpublished payload before lending it as
`&mut [u8]`. `new_with_uninit` avoids zeroing but is unsafe:

```rust
let bytes = PyBytes::new_with(ctx, output_len, |output| {
    encode_into(input, output);
    Ok(())
})?;

// SAFETY: the callback writes every output element before returning `Ok(())`.
let bytes_uninit = unsafe {
    PyBytes::new_with_uninit(ctx, output_len, |output| {
        initialize_every_byte(output);
        Ok(())
    })
}?;
```

The unsafe form lends `&mut [MaybeUninit<u8>]`, which must be fully initialized.
Error and panic paths discard the unpublished object, so they may leave it partially initialized.
Therefore the closure must not panic.

The other initial abstractions are deliberately small. `Bound<PyModule>` can
add converted values and type objects, while `PyType::new_exception` creates
a heap exception type. `PyType` defaults to an untyped class marker for these
general operations; the class-definition section specializes it as
`PyType<C>` for types whose instances contain a Rust payload `C`.


#### Rust/Python conversions

Conversions use one trait per direction:

```rust
pub trait FromPyObject<'a, 'py>: Sized {
    fn extract(
        ctx: &ThreadContext<'py>,
        obj: Borrowed<'a, 'py, PyAny>,
    ) -> PyResult<Self>;
}

pub trait IntoPyObject<'py> {
    fn into_pyobject(
        self,
        ctx: &ThreadContext<'py>,
    ) -> PyResult<Bound<'py, PyAny>>;
}
```

Both are fallible because they may allocate or invoke Python protocols. Their
shared `'py` lifetime prevents attached results from escaping. `FromPyObject` preserves
the owner's `'a` lifetime, allowing zero-refcount borrowed arguments; retaining one
requires `to_owned(ctx)`.

Generic implementations compose. `Option<T>` maps `None`, borrowed handles can
be promoted with an incref, and an existing `Bound<T>` can be returned without
refcount traffic:

```rust
let value: Option<u32> = FromPyObject::extract(ctx, argument)?;
let object: Bound<'py, PyAny> = value.into_pyobject(ctx)?;
```

Generated callbacks accept a convertible value or `PyResult<T>`. Returning `T` means the Rust function
body cannot itself report a Python exception, although converting `T` into a
Python object may still fail. Returning `PyResult<T>` lets the function use `?`
to propagate `PyErrRaised`; only an `Ok(T)` value is converted.


#### Module definition

Modules use PEP 793's `PyModExport` and `PySlot` representation rather than a
legacy `PyModuleDef` initializer. A declarative `export_module!` macro builds
the static method table, ABI declaration, slot array, lifecycle callbacks,
and correctly named export symbol:

```rust
export_module! {
    name: _base64,
    doc: c"Base64 encoding implemented in Rust",
    state: Base64State,
    methods: [standard_b64encode],
    exec: base64_exec,
}
```

The declarative macro expands to inspectable static Rust items.
`#[pyfunction]` emits `ModuleMethodDef<S>`; `export_module!` accepts only
methods carrying its declared `S`, so state mismatches are Rust type errors.

Generated entry points represent a module whose definition is known with a
typed borrowed wrapper:

```rust
pub struct ModuleRef<'a, 'py, S> {
    module: Borrowed<'a, 'py, PyModule>,
    _state: PhantomData<fn() -> S>,
}
```

Only generated `ModuleDef<S>` glue constructs `ModuleRef<S>` and proves the
state type.

Modules always have a typed, per-module state, even if it is empty:

```rust
pub trait ModuleState: Send + Sync + 'static + Sized {
    type Gc: ModuleStateGc<Self>;

    fn new<'a, 'py>(
        ctx: &ThreadContext<'py>,
        module: ModuleRef<'a, 'py, Self>,
    ) -> PyResult<Self>;
}

// SAFETY: implementations must report every strong Python edge, clear each
// edge at most once, and remain valid after partial clearing.
pub unsafe trait ModuleStateGc<S: ModuleState>: 'static {
    const ENABLED: bool;

    fn traverse(value: &S, visit: &Visit<'_>) -> TraverseResult;
    fn clear(value: &mut S, ctx: &ThreadContext<'_>);
}

#[derive(ModuleStateGc)]
struct Base64State {
    error: Option<Py<PyType>>,
}

impl ModuleState for Base64State {
    type Gc = Base64StateGc;

    fn new<'a, 'py>(
        _ctx: &ThreadContext<'py>,
        _module: ModuleRef<'a, 'py, Self>,
    ) -> PyResult<Self> {
        Ok(Self { error: None })
    }
}
```

`Send + Sync` supports concurrent access in free-threaded builds; stored
Python objects use `Py<T>`.

`#[derive(ModuleStateGc)]` emits the associated policy. A sealed field trait
recurses through `Py<T>`, `Option<T>`, and supported containers; Rust-only
state emits `ENABLED = false` and installs no GC callbacks. Manual policy
implementations are unsafe: they must report every strong edge, clear each at
most once, tolerate partial clear, and never block during traversal.

CPython allocates `StateStorage<S>`, an atomic phase plus `MaybeUninit<S>`,
before Rust constructs `S`. The macro rejects types whose alignment exceeds
the allocator's documented guarantee and checks all size arithmetic.
Repeated, reentrant, and concurrent exec calls require these explicit phases:

| Operation | Allowed source phase | Published phase and access |
| --- | --- | --- |
| Claim exec | `Uninitialized` | `Initializing`; any other source raises `SystemError`. |
| Finish `S::new` | `Initializing` | `FailedWithoutValue`, or write `S` and publish `Executing`. |
| Finish exec | `Executing` | `Ready` or `FailedWithValue`; `S` remains live. |
| Read state | `Ready` | Borrow `&S` for the `ModuleRef` lifetime. |
| Traverse | `Executing`, `Ready`, `FailedWithValue`, `Clearing`, `Cleared`, `ClearFailed` | Borrow `&S`; never traverse `Dropping` or `Dropped`. |
| Clear | `Ready` or `FailedWithValue` | Enter `Clearing`, lend `&mut S`, then publish `Cleared` or `ClearFailed`; later clears are no-ops. |
| Free with value | `Executing`, `Ready`, `FailedWithValue`, `Clearing`, `Cleared`, or `ClearFailed` | Enter `Dropping`, drop once, then publish `Dropped`. |
| Free without value | `Uninitialized` or `FailedWithoutValue` | Publish `Dropped` without calling `drop_in_place`. |

The exec hook has the typed signature:

```rust
type ModuleExec<S> = for<'a, 'py> fn(
    &ThreadContext<'py>,
    ModuleRef<'a, 'py, S>,
    &S,
) -> PyResult<()>;
```

Non-panicking guards apply failure phases during unwinding. Stores that
publish an initialized value or completed transition use release ordering;
dependent reads use acquire ordering.

CPython calls clear only after unreachability, and callback borrows cannot
escape, which justifies lending `&mut S`. Derived clear takes each `Py<T>`
before `drop_with(ctx)`, making it idempotent.

Free never retries a panicking destructor. Non-panicking guards still publish
`Dropped` and release the allocation.

Generated modules declare `Py_mod_gil = Py_MOD_GIL_NOT_USED`, the internal
exact-version ABI, the GIL/free-threaded build mode, and
`Py_mod_multiple_interpreters = Py_MOD_MULTIPLE_INTERPRETERS_NOT_SUPPORTED`.
Stable-ABI and subinterpreter support are out of scope.


#### Function definition

Module-scoped, Python-callable Rust functions remain ordinary Rust functions with two
explicit leading parameters: the thread context and the module state.
Python-visible arguments follow them. This interim example accepts exact bytes
pending a buffer API design:

```rust
#[pyfunction(signature = (data, /))]
fn standard_b64encode<'a, 'py>(
    ctx: &ThreadContext<'py>,
    _state: &Base64State,
    data: Borrowed<'a, 'py, PyBytes>,
) -> PyResult<Bound<'py, PyBytes>> {
    // ...
}
```

The macro identifies context and state by position, then emits compiler-checked
uses of their declared types. The state determines `ModuleMethodDef<S>`.
Functions that detach take `&mut ThreadContext`; others take a shared borrow.

`#[pyfunction]` preserves the Rust function body and emits its typed definition,
argument specification, and `METH_FASTCALL | METH_KEYWORDS` trampoline. The
trampoline enters panic containment and `with_current`, validates scoped C
inputs, recovers ready state, binds arguments, invokes Rust, and converts the
result to C before `'py` ends.

The C input is untyped, so private glue validates it against the selected
`ModuleDef<S>` before constructing a callback-scoped `ModuleRef<S>`. Methods
use the same design with `METH_METHOD`.

Panic containment covers entry bookkeeping, parsing, user code, conversion,
and cleanup. A pointer-returning boundary is conceptually:

```rust
// SAFETY: callers must obey `PyCFunctionFastWithKeywords`, provide live
// callback arguments, and invoke this function with an attached thread state.
unsafe extern "C" fn trampoline(
    /* existing PyCFunctionFastWithKeywords arguments */
) -> *mut ffi::PyObject {
    let call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the callback has an attached state and live arguments; the
        // closure consumes every attached value before `'py` ends.
        unsafe {
            with_current(|ctx| {
                let result = call_rust_function(ctx);
                translate_result_to_raw(ctx, result)
            })
        }
    }));

    match call {
        Ok(ptr) => ptr,
        Err(payload) => {
            dispose_panic_payload(payload);
            // SAFETY: unwinding restored attachment; this fixed-message helper
            // does not allocate, invoke user code, or unwind.
            unsafe { set_fixed_callback_panic_exception() };
            std::ptr::null_mut()
        }
    }
}
```

Because `panic_any` payload destruction may panic, `dispose_panic_payload`
drops it under a second catch; a second panic is leaked and aborts. Reporting
uses a fixed unraisable exception message and aborts if it cannot remain non-panicking.

Not every callback ABI can report a normal Python exception, so generated glue
uses a callback-specific policy:

| Callback kind | Panic policy |
| --- | --- |
| Function, method, getter, setter, and constructor | Set a fixed `SystemError` and return the ABI's `NULL` or `-1` error sentinel. |
| Module create | Set a fixed `SystemError` and return `NULL`. |
| Module exec | Commit the appropriate failed state phase, set a fixed `SystemError`, and return `-1`. |
| Module or instance clear | Finish the lifecycle transition, set `SystemError`, and return failure; CPython reports it as unraisable where required. |
| Traverse | Never leave a Python exception pending. A panic is fatal because continuing with an incomplete reachability report can violate GC invariants. |
| `tp_dealloc` and module free | Run mandatory non-panicking cleanup guards first, then report the panic as unraisable when an attached state is available. Abort if cleanup itself cannot be completed safely. |

Before `drop_in_place<C>`, deallocation commits the destroyed phase and
installs guards for the Python allocation and heap-type reference. A panic
therefore cannot cause a second Rust drop or skip mandatory cleanup. Module
state uses the same pattern.


#### Class definition and method definition

A Rust payload type `C` identifies one Python class, its module state, and its
GC policy:

```rust
pub trait PyClass: Send + Sync + 'static + Sized {
    type State: ModuleState;
    type Gc: PyClassGc<Self>;

    fn definition() -> &'static ClassDef<Self>;
}

// SAFETY: implementations must report every strong Python edge, clear each
// edge at most once, and synchronize access according to the class lifecycle.
pub unsafe trait PyClassGc<C: PyClass>: 'static {
    const ENABLED: bool;

    fn traverse(value: &C, visit: &Visit<'_>) -> TraverseResult;
    fn clear(value: &mut C, ctx: &ThreadContext<'_>);
}

#[derive(PyClassGc)]
struct Encoder {
    level: u32,
}

impl PyClass for Encoder {
    type State = ModuleStateType;
    type Gc = EncoderGc;

    fn definition() -> &'static ClassDef<Self> {
        &ENCODER_CLASS
    }
}

static ENCODER_CLASS: ClassDef<Encoder> =
    ClassDef::new(c"example.Encoder")
        .methods(&ENCODER_METHODS)
        .getsets(&ENCODER_GETSETS);
```

This prevents safe code from combining an unrelated payload, module state, or
definition. `#[derive(PyClassGc)]` uses the same sealed field rules as module
state and disables GC slots when no field can own Python references. Manual
implementations are unsafe because they must report every edge, clear it once,
and obey the class's synchronization rules.

`PyType<C = UntypedClass>` and `PyInstance<C: PyClass>` are transparent
markers over this identity. The default represents an ordinary untyped Python
type object.

`ENCODER_CLASS` creates `Bound<PyType<Encoder>>`; module state stores
`Py<PyType<Encoder>>`. Upcasts to untyped `PyType` or `PyAny` are free.
Downcasts must validate the class token. Generated methods, getsets, and
constructors all carry `C`; method state is fixed to `C::State`.

`ClassDef<C>` computes the payload layout with checked arithmetic and installs
lifecycle slots. Types whose alignment exceeds the object allocator's
documented guarantee are rejected before allocation. The allocation records
initialization, clear, and drop phases so callbacks never read uninitialized
or destroyed `C`.

`ClassDef<C>::create(ModuleRef<C::State>)` combines static slots with the
runtime module pointer and returns `Bound<PyType<C>>`. It verifies the unique
`C::definition()`; a second definition requires a Rust newtype. The definition
address is also stored as `Py_tp_token`. `new_instance(payload)` validates that
token before allocation or payload access, then returns
`Bound<PyInstance<C>>`.

Rust structs in classes must be `Send + Sync + 'static`. Python methods receive only `&self`,
never `&mut self`, because aliases can call the same object concurrently in a
free-threaded interpreter. Mutable payload state therefore requires interior
mutability. The rules for a blocking interpreter-aware lock remain an open
decision listed in the appendix; an ordinary `Mutex` must not be presented as
the general solution until those rules are settled.


Python-owning fields enable generated GC slots:

```rust
#[derive(PyClassGc)]
struct Node {
    parent: Option<Py<PyInstance<Node>>>,
    value: u64,
}
```

With a `PyClass` implementation like `Encoder`'s, `Node` sets
`Py_TPFLAGS_HAVE_GC` and installs `tp_traverse` and `tp_clear`. Lifecycle rules
are:

| Operation | Invariant |
| --- | --- |
| Allocate | Write `C` into an untracked object, publish ready, then call `PyObject_GC_Track`. |
| Traverse | Borrow `&C`; only call `visitproc`; never invoke Python or block. |
| Clear | After unreachability, lend `&mut C`; take and `drop_with(ctx)` each edge once; later clears are no-ops. |
| Deallocate | Untrack first. Contain any required clear independently; then commit dropping, drop `C` once, run allocation/type guards, and report the first panic. |

Types are immutable, final, and non-instantiable unless they define `tp_new`.
Finality preserves the `METH_METHOD` defining-class link used to recover typed
module state. Rust factories may always use `new_instance`.

`#[pymethods]` processes four restricted kinds of entries:

- `#[pyfunction]` creates a normal instance method, a `PyCMethod` trampoline,
  and a `ClassMethodDef<C>` whose state parameter is `&C::State`.
- `#[getter]` creates a read-only `ClassGetSetDef<C>`.
- `#[setter]` creates the setter half of a `ClassGetSetDef<C>`.
- `#[new]` creates a `tp_new` trampoline which parses arguments, constructs
  `C`, and a `ClassNewDef<C>` which moves it into a new Python allocation.

All generated slots retain CPython's existing ABI and enter the common context
and panic boundary. The resulting `Py<PyType<C>>` belongs in module state;
`add_type` accepts its safe untyped upcast.


#### Argument parsing

Argument parsing exposes a small PyO3-like signature syntax:

```rust
#[pyfunction(signature = (data, value = 1, /))]
fn checksum<'a, 'py>(
    ctx: &ThreadContext<'py>,
    state: &ChecksumState,
    data: Borrowed<'a, 'py, PyBytes>,
    value: u32,
) -> PyResult<u32> {
    // ...
}
```

The macro checks parameter names and emits a static `ParamSpec`. Supplied
values use `FromPyObject` to extract Rust types; defaults are Rust expressions.

Vectorcall binding uses a fixed stack array and allocation-free keyword
matching. Slots become `Borrowed<'args, 'py, PyAny>` without refcount traffic;
`CallbackArgs` lends `'args`, while `with_current` supplies `'py`. Constructor
tuple/dict parsing fills the same slots and shares conversion code.

Too many, duplicate, unexpected, or missing arguments raise `TypeError`. The
initial grammar grows only as modules require more features.


#### Testing

CI should run `cargo fmt` and
[Clippy](https://github.com/rust-lang/rust-clippy).

A new `xxtestrustapi` module will be introduced to the normal Python test suite suite to ensure the correctness of the
implementation and check invariants of the Rust code.

CI will run a mechanical check that every Python-touching safe `c_api`
wrapper must accept `ThreadContext`; an operation without one needs an audited
thread-state-free justification.

### Appendix: Open decisions and future work

The following items are intentionally kept out of the normative API sections:

- **Subinterpreters.** Future support requires an interpreter-identity model
  for contexts, owning handles, module state, and process-global Rust values.
- **Buffer protocol.** No safe `PyBuffer` API is included until the design
  distinguishes read-only and writable exporters; specifies contiguity,
  format, alignment, ownership, and release on every exit path; and prevents
  Rust slices from racing with Python mutation or escaping across detach and
  reentrant calls. Copying from an exact immutable bytes object is the safe
  interim input path.
- **Interpreter-aware locking.** The design must either provide a lock which
  cooperates with attachment, the GIL, and free-threaded stop-the-world pauses,
  or prohibit blocking, detaching, calling Python, and reaching safepoints
  while an ordinary Rust lock guard is held. Poisoning and traversal behavior
  also require an explicit policy.
- **Detached destruction.** Consider a deferred decref queue if detached
  destruction is needed in practice.
- **Explicit-state C optimization.** Benchmark explicit-`PyThreadState *`
  variants for TLS-heavy hot paths; this would not change the safe Rust API.
- **Rust runtime and panic strategy.** The initial API requires `std` and
  `panic=unwind` so generated callbacks can contain panics, complete lifecycle
  cleanup, and report errors. A `panic=abort` build remains ABI-safe but cannot
  provide that recovery behavior. Any future `no_std` configuration must use
  panic-abort or provide another complete no-unwind boundary.
