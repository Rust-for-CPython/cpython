# Rust for CPython: API Design for 3.16

## Overview

This document designs a safe Rust API for in-tree CPython extension modules,
with `zlib` as the first full migration target.

The wrapper enforces the lifetime, aliasing, and thread-safety rules missing
from raw FFI. It requires CPython support for context enforcement, protection of active thread states, and deferred reference release
without attachment. The appendix lists prerequisite CPython fixes.

## Glossary

Standard Rust terms keep their usual meanings; in particular, `&T` is a
shared reference and `&mut T` is an exclusive (mutable) reference. The terms
below have design-specific meanings.

- **Entry:** a call into Rust from Python or the C API: a module function,
  method, or slot callback. Generated **entry glue** wraps the Rust
  function; its private **entry guard** keeps resources alive from argument
  parsing through cleanup. See [Entry glue](#entry-glue).
- **Execution phase:** the circumstances under which CPython calls Rust:
  ordinary execution, finalization cleanup, or GC traversal. Each permits
  different operations; see [Execution phases](#execution-phases).
- **Capability:** a value or reference proving permission to perform an
  operation: normally a borrow of `ThreadContext`, with restricted
  alternatives for GC traversal and reference release.
- **Class payload:** the Rust data stored in a Python instance. See
  [Classes](#classes).
- **Leaf operation:** an operation that preserves attachment and cannot
  re-enter Python, invoke arbitrary Rust callbacks, run deallocators, or
  reach a safepoint. It may still call other C functions. See
  [The thread context](#the-thread-context).
- **Safepoint:** a point in a runtime operation where the thread may pause
  or service pending work, for example a stop-the-world request.
- **Exclusive context access:** an `&mut ThreadContext` borrow, required by
  operations that can run callbacks or change attachment. It is Rust's
  exclusive borrow, not a lock on the interpreter or its objects.
- **Available / suspended context:** safe Rust code may use an available
  context. Suspension temporarily makes it unavailable around a reentrant
  call while keeping the original thread state protected; it does not by
  itself detach the thread. See [Scope transitions](#scope-transitions).
- **Interpreter guard:** a PEP 788 `PyInterpreterGuard`, which prevents an
  interpreter from finalizing until it is closed. Each ordinary entry owns
  one inside its private entry guard. See
  [Interpreter guards and entry lifetime](#interpreter-guards-and-entry-lifetime).
- **Admission:** permission to acquire an interpreter guard or submit a
  reference for release. Closing admission prevents new ordinary entries
  and submissions; finalization callbacks use separate cleanup authorization.
- **Transition check:** runtime enforcement
  that rejects a thread-state transition or nested entry that would
  invalidate an available Rust context. See
  [Transition checks](#transition-checks).
- **Release sink / queue:** a sink buffers references awaiting `Py_DECREF`
  for one entry; the queue holds them for an interpreter. **Draining**
  performs the deferred decrefs. See
  [Deferred reference release](#deferred-reference-release).
- **Foreign code:** C or Rust code outside this API, including other Rust
  wrappers of the C API. See
  [Detachment and foreign wrappers](#detachment-and-foreign-wrappers).

## Goals

In priority order, the API should:

1. be sound on GIL and free-threaded builds, including subinterpreters with
   separate GILs;
2. support every PEP 11 platform;
3. approach equivalent C performance where safety permits;
4. feel familiar to PyO3 and C API users; and
5. use modern APIs such as `PyModExport`.

It encodes CPython contracts in the type system where practical, preserves
Rust's initialization, aliasing, and concurrency invariants, and keeps
compile times under control.

**Non-goals:** full C API parity, a stable ABI, and sharing Python objects
between interpreters. Wrappers are added as Rust modules need them.

### Runtime lifetime

Before reinitializing Python after `Py_FinalizeEx()`, embedders must ensure
that no old handles, traced slots, interpreter views, or release submissions
can be used or dropped afterward, including those retained in globals or
worker threads. They must dispose of these values and quiesce their producers.

This safety requirement is unenforced; violating it can cause undefined
behavior. Given the expected rarity of such retention, the design accepts
this risk to avoid generation bookkeeping. Reinitialization is otherwise
permitted.

### Safety rules

- Every `unsafe` has a `// SAFETY:` comment stating its proof obligation.
- No panic may escape a non-unwinding C ABI.
- Generated code never expands author-supplied expressions, defaults, or
  calls inside an `unsafe` block; it binds them in safe items first.
- A trait whose implementations unsafe code trusts (a type check, type
  pointer, or class identity) is `unsafe` or sealed. Otherwise a crate with
  `#![forbid(unsafe_code)]` could cause undefined behavior by implementing
  it.
- A type parameter carrying an identity that unsafe code relies on (a class
  payload, module state, or traced slot's contents) is invariant.
  `Payload<fn(&str)>` is a subtype of `Payload<fn(&'static str)>`, so a
  covariant parameter would let safe code relabel one as the other.
- A typed handle is created only after a type check, or from a C function
  whose result type Python code cannot choose. (A metaclass, for example,
  decides what `type(name, bases, dict)` returns.)

## API design

1. [Bindings and the FFI boundary](#bindings-and-the-ffi-boundary)
2. [Interpreter identity and lifetime](#interpreter-identity-and-lifetime)
3. [The thread context](#the-thread-context)
4. [Python object handles](#python-object-handles)
5. [Deferred reference release](#deferred-reference-release)
6. [Exceptions](#exceptions)
7. [Concrete objects and conversions](#concrete-objects-and-conversions)
8. [Modules](#modules)
9. [Argument parsing](#argument-parsing)
10. [Functions](#functions)
11. [Classes](#classes)

### Bindings and the FFI boundary

`cpython-sys` generates raw bindings at build time from `Python.h`, the
internal headers, and the active `pyconfig.h`, capturing layout that depends
on platform, target, debug, and free-threading configuration. Bindgen
allowlists public and private `Py*`/`_Py*` names; constructs it cannot
translate, such as macros, are reimplemented in Rust.

Python objects are mutable through shared pointers, so the binding uses
`UnsafeCell`:

```rust
#[repr(transparent)]
pub struct PyObject(UnsafeCell<_object>);
```

All C calls go through a private `c_api` module. Every safe operation that
may touch Python requires a capability (see
[The thread context](#the-thread-context)); an operation without one needs
an audited proof that its whole call path never accesses a thread state.
Until Rust offers a `Drop` that can take a context, `Drop` never obtains a
capability; it only submits a release.

### Interpreter identity and lifetime

#### Interpreter IDs

The API uses `PyInterpreterState_GetID()`: a nonnegative `int64_t` (`i64`
in Rust), with zero identifying the main interpreter. IDs are not reused
within one runtime, but reset on reinitialization. The
[runtime lifetime requirement](#runtime-lifetime) therefore replaces a
separate generation token. PEP 788 views resolve by the same IDs and do
not keep interpreters alive.

An ID names the interpreter under whose API contract a reference was
acquired, not the one that allocated the object.

Objects carry no interpreter association. Generated glue reads the ID from
the attached thread state's interpreter, with no allocation, search, or
lock, and conversions preserve it.

#### Interpreter guards and entry lifetime

An interpreter ID proves identity, not liveness. Every ordinary entry uses
a [PEP 788 guard](https://peps.python.org/pep-0788/#interpreter-guards) and
`PyThreadState_Ensure(guard)` to establish attachment before exposing a
context or payload borrow. Generated Python callbacks acquire the guard
with `PyInterpreterGuard_FromCurrent()`; native callers use an explicit
interpreter view and `PyInterpreterGuard_FromView()`.

The private entry guard owns the successful `PyThreadStateToken` and keeps
the interpreter guarded through its release. With a separately acquired
guard, it calls `PyThreadState_Release(token)` before closing the guard;
`PyThreadState_EnsureFromView` manages that guard implicitly. Both paths
finish conversion, unwinding, release processing, and exception reporting
before releasing the token, so the guard covers thread-state cleanup too.

Acquisition may invoke allocator hooks or fail because admission is closed
or allocation failed. It therefore precedes context availability; failure
follows the [entry-glue policy](#entry-glue), never an unguarded fallback.
Cleanup callbacks use [cleanup authorization](#cleanup-authorization)
without acquiring a guard or an ensure token. Pre-finalization waiting
still admits guarded nested entries.

Guards cover indirect detachment through Python as well as explicit
`detach` and `interop` calls. This protects:

- **Ancestor borrows:** if A lends `&S` through scoped TLS to nested entry
  B, A's guard remains live while B detaches and reads it.
- **Deferred-counted objects:** on free-threaded builds, finalization cannot
  remove the frame roots keeping modules, functions, and payloads alive.
- **Scoped workers:** `&S`, `&C`, and `&[u8]` may cross threads under their
  Rust bounds, provided workers finish before the borrowing scope returns.
  Waiting for workers that may need Python requires detachment. Contexts
  and scoped handles remain `!Send + !Sync`.

Guards neither retain individual objects nor establish attachment or
synchronization; ordinary ownership and GC rules still apply. Stored
`Py<T>` handles never hold guards, which could make module state prevent
the shutdown needed to destroy it.

The baseline uses the public API's allocation, shared counter, and admission
barrier. The release queue joins that barrier (see [Shutdown](#shutdown)).

#### Shutdown progress and guard ownership

PEP 788 already makes finalization wait for outstanding guards. Holding one
per Rust entry therefore lets a blocked daemon thread delay shutdown
indefinitely. The private entry glue prevents safe code from closing or
forgetting these guards.


#### Cleanup authorization

After guard admission closes, CPython's finalization callbacks and their
synchronous descendants use **cleanup authorization**. It requires the
attached state to be the interpreter's recorded finalizing state; other
entries receive `EntryError`. All other states are marked shutting down
before the world restarts. Authorization cannot be manufactured and ends
at the [final drain point](#shutdown).

Cleanup may detach and interoperate: the finalizer cannot advance until its
synchronous callback returns. Other threads hang in `take_gil` or
`tstate_wait_attach`; fresh threads hang in `PyGILState_Ensure`. Cleanup
must therefore never wait for a thread that needs the interpreter.
It uses the already-attached finalizing state directly;
`PyThreadState_EnsureFromView` would attempt a new guard acquisition and
fail after admission closes.

### The thread context

`ThreadContext<'py>` is a scoped, exclusive capability for one exact thread
state on the current OS thread. Whenever safe code can use it, that state is
attached and protected against finalization.

```rust
pub struct ThreadContext<'py> {
    ptr: NonNull<ffi::PyThreadState>,
    interpreter_id: i64,
    entry: NonNull<EntryState>, // private, stable storage in the entry guard
    #[allow(clippy::type_complexity)]
    _marker: PhantomData<(fn(&'py ()) -> &'py (), *mut ())>,
}
```

The marker makes `'py` invariant and the context `!Send + !Sync`. It is
neither `Copy` nor `Clone`, and only generated entry glue constructs it.
`'py` is a fresh lifetime per entry, and `Bound` and `Borrowed` are
invariant in it too, so separate entries cannot mix scoped values even
within one interpreter.

| Context argument | Permitted operations | Examples |
| --- | --- | --- |
| `&ThreadContext<'py>` | Audited [leaf operations](#glossary): no attachment changes, callbacks, deallocators, or safepoints. | Checked binding, ownership-preserving casts, immutable bytes access, audited incref. |
| `&mut ThreadContext<'py>` | Operations that may reenter, allocate through reentrant paths, decref, or change attachment. | Extraction, object creation, attribute access, destruction, release draining, `detach`. |

The audit covers failure handling, allocator hooks, and cleanup. Shared
operations return Rust errors without setting Python's error indicator;
raising requires exclusive access. New wrappers default to exclusive, and
blocking acquisition is never a leaf operation. Attachment does not prevent
concurrent mutation on free-threaded builds.

Holding `&mut ThreadContext` does not make the context unavailable. A
private wrapper **suspends** it only around the actual reentrant C call or
closure, and never passes a suspended context to author code.

#### Execution phases

An **execution phase** determines which operations CPython permits when it
calls Rust. It is independent of whether a thread is attached or a context
is available or suspended.

| Execution phase | Permitted operations |
| --- | --- |
| Ordinary execution | The entry holds a PEP 788 interpreter guard throughout and may use whatever its shared or exclusive context borrow allows, including explicit detachment. |
| Finalization cleanup | Callbacks use [cleanup authorization](#cleanup-authorization): they may detach and interoperate, but must not wait on a thread that needs the interpreter. |
| GC traversal | Callbacks use the restricted GC visitor operations, without a general-purpose `ThreadContext`, and must obey the [GC slot synchronization](#gc-slot-synchronization) rules. |

After the [final drain point](#shutdown), no entry can obtain a context.
The module and instance lifecycle states described later (such as `Ready`
and `Cleared`) track initialization and destruction separately.

#### Availability flag and thread-state protection

Borrowing and leaf audits cannot constrain foreign code that detaches or
reenters while an outer context remains accessible through scoped TLS.
CPython therefore enforces two runtime invariants; interpreter guards alone establish neither:

- An **availability flag** allows at most one available context per OS
  thread, across all extensions and interpreters. Lexically scoped entry
  and restoration guards need only one flag. Only the owning thread
  modifies the flag, so it needs no atomics.
- A synchronized **protection count** per `PyThreadState` prevents clearing
  or deletion, even from another thread, while an ordinary or cleanup entry
  is active. Protection persists through suspension and interpreter
  switches. `PyThreadState_Ensure` handles attachment and its matching
  release; it does not replace this protection against explicit clearing
  or deletion, or the availability checks.


#### Scope transitions

1. **Enter:** require the flag clear and determine the
   [phase](#execution-phases). An ordinary entry acquires a guard and calls
   `PyThreadState_Ensure`; cleanup verifies authorization on the attached
   finalizing state instead. Expose no context or payload borrow during
   setup. Protect the resulting thread state before further reentrant work,
   then set the flag and publish the context. On failure, follow the
   [entry-glue policy](#entry-glue).
2. **Suspend:** clear the flag before a reentrant operation. Refusal by the
   initial checks leaves attachment and the flag unchanged without raising.
   The restoration guard keeps the context's state. Suspension does not
   broaden the C operations the execution phase permits.
3. **Check:** attach, detach, swap, deletion, and generated entry paths
   enforce the [transition checks](#transition-checks) in every build,
   including when no state is attached.
4. **Resume:** require the flag clear and the context's state attached,
   apply the [exception policy](#exceptions), then set the flag. If a
   foreign call failed to restore the state, fail fatally rather than
   attach over an unknown state.
5. **Exit:** clear the flag, drain or transfer releases, and retire the empty
   sink. Finish the scoped cleanup, result/error transfer, and panic reporting
   while still attached to the context's state. Remove its protection, then
   release the ensure token, which may delete that state and restores the
   caller's prior attachment (possibly none). Close the explicit interpreter
   guard last. Cleanup entries have neither a token nor a guard to release.

Private entry and restoration guards cannot be forgotten by safe code.
Forgetting a public handle may leak a reference but cannot alter runtime
protection. The entry guard keeps its release sink at a stable stack address.

#### Transition checks

The availability flag must be clear on:

- the internal detach path (reached by every public release and swap),
  deletion and clearing of the current thread state, `Py_MakePendingCalls`,
  and the deprecated `PyEval_ReleaseLock`;
- entry into generated glue, since a nested entry under an available
  context would create a second context that could detach independently of
  the outer context's borrows.

Clearing or deleting any thread state also requires a zero protection
count, regardless of the caller's availability flag. Rust wrappers check
permission before `PyEval_SaveThread`; violations by foreign code fail
fatally before changing attachment or invalidating a protected state.
Waiting instead would deadlock the current thread.

Two paths are exempt: `_PyParkingLot_Park`, through a private thread-local
marker, and the `_PyThreadState_Suspend` stop-the-world safepoint. A path
is exempt only if no callback or Rust access can run before the exact
state is restored. The parking lot calls the public
`PyEval_ReleaseThread`/`PyEval_AcquireThread`, so the private marker is
what distinguishes it from explicit detachment. Lock paths that call
`Py_MakePendingCalls` for signals require exclusive context access and
suspension instead. Automatic release draining stays deferred during an
exempt restoration while a context is available; otherwise a decref would
introduce callbacks into a path justified by their absence.

The check does not stop foreign code from running Python on the attached
state (`PyObject_Call` from another library, or `PyErr_CheckSignals`),
which need not violate that library's contract. Every Rust boundary such
code can reach enforces the flag in every build: a nested entry or
forbidden transition fails fatally, GC clear of a Rust object is refused
before constructing a context (leaking the cycle), and payloads are dropped
without a context as always. Foreign code can also set an exception without
a transition; wrappers handle this through the
[exception policy](#exceptions).

#### Entry glue

Ordinary entry uses the [PEP 788 attachment APIs](https://peps.python.org/pep-0788/#attaching-and-detaching-thread-states):

- Generated Python callbacks already have an attached state under their C
  API contract. `PyThreadState_Ensure(guard)` reuses that state for the
  guarded interpreter and returns a token for the matching release.
- Native entry supplies an explicit interpreter view, acquires its guard,
  and uses the same ensure/release path. It never implicitly selects the
  main interpreter through `PyGILState_Ensure`.
- `PyThreadState_EnsureFromView(view)` can combine native guard acquisition
  and attachment. Its successful token carries the guard's lifetime;
  release it once, without separately closing the implicit guard.

The token is opaque: it records how to undo attachment, not necessarily the
state that was attached. After success, glue reads the attached state to
construct `ThreadContext`. Each successful ensure has exactly one matching
release on the same thread, in nesting order, including on panic or later
setup failure. The ensured state must be restored before release. No scoped
handle, borrowed view, or pending Python exception may escape a native
entry; results and errors must be converted to Rust values or unbound
handles before release.

A simplified view of the entry glue for a function call:

```rust
// SAFETY: only generated C callbacks with an attached state and valid inputs
// may enter; the implementation enforces availability, checks the phase,
// and establishes a guarded or cleanup-authorized context, or refuses entry.
unsafe fn with_current<R>(
    f: impl for<'py> FnOnce(&mut ThreadContext<'py>) -> R,
) -> Result<R, EntryError>;
```

`for<'py>` makes the lifetime fresh, so `R` cannot depend on it, and macros
reject author functions that name a specific lifetime such as `'static`.
Generated wrappers convert results to raw C values before returning.
`EntryError` is a Rust error value; callback-specific glue reports it
without manufacturing a context after admission closes.

Ensure calls return `NULL` on failure without setting a Python exception.
Glue must not release a failed token and must close any guard it acquired
separately. Native entry reports setup failure as a Rust error without
assuming attachment; it never runs the body on failure. For generated
callbacks, guard or attachment setup failure follows the callback's ABI:

- Functions, methods, constructors, and module initialization return their
  ABI's failure sentinel without calling the normal Rust body. They preserve
  an acquisition exception, or report allocation failure as `MemoryError`
  through private reporting authorization when no exception was set.
- Clear callbacks leave payloads and slots untouched. Module clear returns
  0 with an exception so CPython still clears the module dict; instance
  clear returns failure. Refusal may leak a cycle.
- `tp_dealloc` and module free save and clear the incoming exception before
  entry setup. On failure they still perform mandatory teardown
  without a context: commit the dropping state, drop initialized Rust data
  exactly once, and run the allocation/type cleanup guards. They skip
  context-requiring clear callbacks; field drops submit releases without
  attaching. Errors follow the [deallocator exception policy](#dropping).
  Teardown must complete safely or abort.

These failure paths retain any established private setup protection until
their cleanup completes, and never hand a context to a destructor. GC
traversal uses its restricted visitor capability and does not allocate an
interpreter guard. After the final drain point, failure diagnostics must
not invoke Python: use a native diagnostic or abort, while preserving
mandatory no-context teardown.

#### Detachment and foreign wrappers

```rust
impl<'py> ThreadContext<'py> {
    pub fn detach<R>(
        &mut self,
        f: impl FnOnce() -> R,
    ) -> PyResult<'py, R>;

    pub fn interop<R>(
        &mut self,
        f: impl FnOnce() -> R,
    ) -> PyResult<'py, R>;

    pub fn drain_releases(&mut self);
}
```

- **`detach`** suspends, detaches, runs `f` on the same thread, and restores
  the state even on panic. Nested and cleanup entries may detach. Their
  [lifetime protection](#interpreter-guards-and-entry-lifetime) remains in
  force, so `f` may use entry-borrowed `&S`, `&C`, and `&[u8]` under their
  owner lifetimes. Waiting for another thread that needs Python requires
  detachment to avoid GIL or stop-the-world deadlocks; during cleanup that
  thread is hung, so waiting deadlocks regardless.
- **`interop`** suspends without detaching, so `f` can use a foreign safe
  wrapper that manages its own attachment, including switching
  interpreters. The interpreter guard or cleanup authorization and the
  thread-state protection remain in force. The boundary verifies restoration
  of the incoming state, and nested entries obey availability rules. Foreign
  wrappers must uphold their own safety contracts.
- **`drain_releases`** services queued releases (see [Draining](#draining)).

A pending exception is the only refusal: both APIs return it as `Err`
before suspending. On return, an exception left by `f` replaces its result
with `Err`, before availability is restored. See [Exceptions](#exceptions).

The closures receive no context and need no `Send` bounds: they run on the
same thread, and the proof is exclusive access to the capability. Handles
may move in and out but remain unusable without an available context.

This closes the holes in PyO3
[#2141](https://github.com/PyO3/pyo3/issues/2141) and
[#3640](https://github.com/PyO3/pyo3/issues/3640), where `SendWrapper` or
scoped TLS let code reach Python without proof of attachment. Here a shared
context reference stashed in TLS blocks the exclusive borrow that detaching
needs, a `RefCell` holding the context fails a reentrant borrow, and
foreign detachment and unsuspended nested entry are rejected by runtime
enforcement in every build. Likewise a TLS hook running while a C call has
switched to interpreter B cannot use A's context, which stays exclusively
borrowed.

### Python object handles

| Handle | Owns a reference | Lifetime | Auto traits | Drop |
| --- | --- | --- | --- | --- |
| `Borrowed<'a, 'py, T>` | No | owner `'a` and attachment `'py` | `Copy`, `!Send`, `!Sync` | none |
| `Bound<'py, T>` | Yes | guarded entry `'py` | `!Send`, `!Sync` | enqueue in entry release sink |
| `Py<T>` | Yes | unbound, for stored state | `Send`, `Sync` | enqueue by interpreter ID |

`Borrowed` stores only a pointer; its owner supplies `'a` and the context
supplies `'py`. `Bound` stores the pointer and a pointer to its entry's
release sink, which records the interpreter ID. `Py<T>` stores the pointer
and an `i64` interpreter ID. Both owning forms are two words on 64-bit
targets. The entry's `'py` lifetime proves association, not permission:
handles can exist while the context is suspended but cannot be used
without an available context.

#### Binding

Every safe `Py<T>` operation that touches the object (binding, downcasting,
cloning, conversion, destruction) first compares its interpreter ID with the
context's, before dereferencing anything. Mismatch returns the Rust error
`InterpreterMismatch` without setting Python's error indicator. This also
rejects expired handles even if a later subinterpreter reuses their address.
Reuse of IDs across runtime reinitialization is covered by the
[runtime lifetime requirement](#runtime-lifetime).

```rust
impl<T> Py<T> {
    pub fn bind<'a, 'py>(
        &'a self,
        ctx: &ThreadContext<'py>,
    ) -> Result<Borrowed<'a, 'py, T>, InterpreterMismatch>;
}
```

Scoped handles share the context's invariant `'py`, so they need no
repeated checks. Moving between forms is explicit:

```rust
let stored: Py<PyType> = exception_type.unbind();
let current: Borrowed<'_, 'py, PyType> = stored.bind(ctx)
    .map_err(|err| err.raise(ctx))?;
let retained: Bound<'py, PyType> = current.to_owned(ctx);
```

Owning handles do not implement `Clone`; `clone_ref(ctx)` and
`to_owned(ctx)` make increfs explicit. Raw-pointer constructors are private
and prove non-nullness, identity, liveness, and ownership. Upcasts,
unbinding, and transfers preserve the interpreter ID.

#### References into Python memory

Rust references into Python-owned memory (byte slices, class payloads,
module state) are bounded by both the owner's lifetime and the entry scope.
A static `Py<T>` cannot yield a `'static` `&[u8]`, `&C`, or `&S`, and the
entry cannot return such references. A view that needs continuous
attachment also borrows the context.

Attachment is not a lock: other threads and foreign wrappers can mutate
Python objects. Shared class and module payloads must be `Send + Sync`.

#### Dropping

```rust
object.drop_with(ctx)?;
```

`drop_with(&mut ctx)` releases directly, with exclusive context access and
the context suspended during the decref.

Ordinary `Drop` never decrefs, attaches, or switches states, even on the
right interpreter: a finalizer could reenter Python while a shared context
is reachable through TLS. `Bound::drop` appends to its entry's sink;
`Py::drop` submits to its interpreter's queue. Neither reads the object,
and `Drop` must not unwind. Sealed markers for immortal singletons (`None`,
booleans) skip release entirely.

**Deallocator exception policy.** Explicit release, draining, `tp_dealloc`,
and module free preserve the exact incoming exception, including none.
CPython debug builds abort if a deallocator changes the exception. After the final
drain point, diagnostics must not invoke Python.

#### Markers and GC

`PyAny` is the marker for any Python object; specific markers like `PyBytes`
share its transparent layout, so upcasts are free. There is no `&mut PyAny`,
since Python objects can change through aliases.

### Deferred reference release

#### Entry sinks

Each entry has a release sink with a small inline buffer, owned by the entry
guard. `Bound::drop` appends to it with no allocation, lock, or Python call.
When it fills, it transfers a batch to the interpreter queue or obtains
overflow storage, without calling Python, attaching a thread state, or
decrefing. If both fail, only the incoming reference is abandoned; accepted
ones are kept.

**Aliasing invariant:** no `&mut` to sink storage may be live across an
operation that can drop a handle into that sink. Pushes use interior
mutability, and draining extracts a batch before decrefing.

`Py::drop` always uses the interpreter queue, even inside a same-interpreter
entry; `drop_with` avoids the queue when promptness matters. Exit drains or
transfers every buffered reference before retiring the sink, draining
locally if transfer fails. After admission closes, authorized cleanup sinks
overflow through a private cleanup route.

#### The release queue

CPython owns one release queue per interpreter lifetime. It accepts
references without an attached thread state through a new private API:

```c
typedef enum {
    _Py_RELEASE_QUEUED,
    _Py_RELEASE_CLOSED,
    _Py_RELEASE_EXPIRED,
    _Py_RELEASE_NO_MEMORY,
} _PyDeferredReleaseResult;

_PyDeferredReleaseResult _PyObject_TryDeferDecRef(
    int64_t interpreter_id,
    PyObject *owned_reference);
```

Only `_Py_RELEASE_QUEUED` consumes the reference: the queue takes it over
without an incref, and the producer must not also decref it. Failure
consumes nothing, raises nothing, and does not inspect the object.

Submission works from any OS thread (detached, in another interpreter,
or after `Py_FinalizeEx`). It must not read the object, touch its refcount,
call Python, attach, panic, or wait for the target interpreter. Its
allocation must not call Python or attach a thread state: `PyMem_RawMalloc`
does not qualify, because under tracemalloc its hook calls
`PyGILState_Ensure`; C `malloc` and an OS mutex do.

A process-lifetime registry maps interpreter IDs to queue endpoints and
rejects late submissions after `Py_FinalizeEx()`. Lookup, admission, and
notification synchronize with retirement, which removes the endpoint and
waits for in-flight producers before freeing the interpreter. A missing ID
means expired. Before reinitialization reuses IDs, all old endpoints must
be retired and producers quiesced under the
[runtime lifetime requirement](#runtime-lifetime); the registry cannot
distinguish stale submissions carrying a reused ID.

#### Notification

After submitting a reference, a producer must notify the interpreter that
its release queue needs attention. The notification cannot use a pending-call
array entry, because that fixed-size array may be full. All eight
eval-breaker bits are already assigned, so the design reuses the existing
calls-to-do bit rather than adding another.

Each interpreter has a separate `releases_pending` flag. A producer sets
this flag when it submits work. When the flag changes from clear to set,
the producer also sets the calls-to-do eval-breaker bit, requesting that
the interpreter check its release queue. Further submissions while the
flag is set need no additional notification. This does not consume a slot
in the pending-call array.

A producer attached to the target interpreter sets its own thread state's
eval breaker. Detached producers and those attached to another interpreter
signal the target's thread states while holding the thread-list lock.
GIL builds also check `releases_pending` when acquiring the GIL.

Each drain triggered by eval-breaker handling or GIL acquisition processes
at most a bounded number of queue nodes, so continuous submissions cannot
keep the interpreter draining indefinitely. If work remains, or draining
is deferred because a Rust context is available, the consumer keeps the
work marked pending and requests another check.

#### Waking the finalizer

A finalizer waiting for guards or submissions must be woken without
attachment. Releasing the last guard may allow immediate interpreter
deletion, so the wake word and waiter count live in process-global storage.
After releasing lifetime protection, producers touch only that storage.
Wake events are setting `releases_pending`, closing the last PEP 788 guard,
and the in-flight submission count reaching zero.

If waiters exist, a waker increments the word and calls
`_PyParkingLot_UnparkAll`: waking only one could select the wrong
interpreter's finalizer. The finalizer registers first, then repeatedly
reads the word, services releases, checks counts, and parks expecting the
word it read. Sequentially consistent atomics prevent lost wakeups.

The loop must also run pending calls before parking, and scheduling one
must wake finalizers. Otherwise a guard holder waiting on a call scheduled
after the last `_Py_FinishPendingCalls` deadlocks (see the appendix).

#### Draining

Draining requires exclusive context access and suspension via
`ctx.drain_releases()`, or private release authorization between scopes.
Eval-breaker handling and GIL acquisition check availability before draining
in every build; if a context is available, they defer draining and preserve
the pending notification. This includes reattachment through the exempt
parking-lot and stop-the-world paths.

A consumer drains while attached. An explicit drain remains covered by its
entry's interpreter guard or cleanup authorization. An automatic consumer
outside an entry does not acquire a guard merely for its batch; if a decref
callback detaches and shutdown hangs that unguarded consumer, the remaining
references leak, as references held by hung C code do today. Any Rust entry
reached by a decref acquires its own guard or cleanup authorization. The
consumer releases locks that do not require attachment before decrefing.
Each reference is released exactly once, attached to its own interpreter;
until then it counts as an external strong reference for GC, so moving an
edge out of a GC field removes the field's edge before submitting the
release.

Deferred release promises neither destruction before `Drop` returns nor
destruction on the dropping thread. Resources needing prompt release should
expose an explicit close method.

#### Shutdown

1. **Open:** the queue accepts and drains normally.
2. **Pre-finalization:** the finalizer waits for threads, pending calls,
   and PEP 788 guards, including those owned by Rust entries, while the
   interpreter is intact. It services releases and releases its GIL and
   locks while waiting. Admission remains open so guarded entries can make
   nested calls; an ordinary Rust entry must finish its cleanup and close
   its guard before the finalizer can close admission.
3. **Close admission:** queue admission closes with the existing
   finalization-guard barrier. The final check covers queued releases and
   in-flight submissions alongside threads, pending calls, and guards, with
   no check-then-enqueue gap. This integrates with
   `make_pre_finalization_calls` rather than running after it with the world
   stopped. Afterwards, normal entries and consumers are refused.
4. **Cleanup drains:** authorized finalization callbacks drop into a cleanup
   sink. External submissions get `CLOSED`, or `EXPIRED` after retirement.
   Cleanup sinks drain to completion after module finalization, before
   interpreter clear, and within it before the last collection, after
   `_PyGC_Fini`, and at the final drain point, covering drops from
   module/type cleanup, the last collection, `gc.garbage`, and teardown of
   warnings state, `sysdict`, and `builtins`.
5. **Final drain point:** right after the interpreter empties `sysdict` and
   `builtins`, before it releases them and clears its common constants.
   After that, running any bytecode is unsafe (even `return None` loads a
   common constant), so cleanup authorization ends: no entries, no
   consumers, and the cleanup route closes. Later callbacks degrade safely:
   deallocation drops payloads without a context, clear leaks, and functions
   raise through the plain C API. Every reference accepted before this point
   is released; none is accepted after.

If the interpreter is cleared from another interpreter's thread state, no
drain can run and the queue is abandoned at retirement.

#### Abandonment

On `CLOSED`, `EXPIRED`, or `NO_MEMORY`, `Drop` abandons the reference and
records a diagnostic without calling Python, attaching a thread state,
allocating, or panicking. It never retries in another interpreter. This can
leak but never exposes an expired object. Since safe Rust can forget
handles, correctness never depends on destructors running. Reclaiming every
retained `Py<T>` would need revocable ownership records on every handle
(see the appendix).

#### Fork

Fork support with active Rust entries is still TBD.

### Exceptions

```rust
#[must_use]
pub struct PyErr<'py> {
    exception: Bound<'py, PyBaseException>,
}

pub type PyResult<'py, T> = Result<T, PyErr<'py>>;
```

`PyErr<'py>` owns a `Bound` exception rather than leaving it pending on the
thread state. All constructors require a context and preserve its invariant
`'py`; dropping the error queues a release without leaving a pending
exception. To retain an error beyond the entry, `unbind()` it as
`Py<PyBaseException>` and later rebind with an interpreter-ID check.
[`#[must_use]`](https://doc.rust-lang.org/std/attribute.must_use.html) warns
when an error is ignored.

Exception types provide `raise(&mut ctx, message)`, which returns an owned
`PyErr`; the exception is set only when the generated boundary restores it:

```rust
fn checked_size<'py>(
    ctx: &mut ThreadContext<'py>,
    size: usize,
) -> PyResult<'py, i32> {
    i32::try_from(size)
        .map_err(|_| PyOverflowError::raise(ctx, "size is too large"))
}
```

Before an error-sensitive C call, the wrapper captures any pending exception
and returns it without making the call. Leaf operations may preserve the
indicator; release and deallocation use their save/clear/restore policy.
On failure, the wrapper immediately takes the exception with
`_PyErr_GetRaisedException` while controlling the originating state:

```rust
// After checking for a pending exception and making the C call,
// with the exclusive borrow held:
if ptr.is_null() {
    // SAFETY: the audited C API's NULL result guarantees an exception on
    // this exact, restored state. Capture transfers the raised reference.
    Err(unsafe { take_raised_exception(call_state) })
} else {
    // Require a clear indicator before transferring the owned result.
}
```

Ambiguous sentinels also check the error indicator. A missing exception
becomes `SystemError`. Success with a pending exception discards the result,
with exclusive context access and the context suspended, and returns the
captured exception. Failures while creating or reporting an error must
leave an appropriate exception such as `MemoryError`.

At a callback boundary, a pending exception turns `Ok` into `Err`, even if
author code made no further wrapped calls. An explicit `Err` takes priority;
secondary exceptions are unraisable. The invariant `'py` excludes errors
from other entries, so no interpreter-ID check is needed. Restoration may
decref the previous exception and requires exclusive access and suspension.

Safe foreign code can set the error indicator without changing attachment.
Function entries therefore capture unexpected pending exceptions before
author code; deallocators [save and restore them](#dropping).

Suspended author closures, including bytes builders, use the same capture
policy. Discarded results and panic payloads are dropped while suspended,
and the indicator is checked again afterwards. During unwinding, restoration
captures secondary exceptions for later unraisable reporting at the boundary;
it neither replaces the original panic nor unwinds itself.

The built-in exception marker trait is sealed because its type pointer goes
straight to C. Custom exception types stored as `Py<PyType>` need checked
binding and `PyExceptionClass_Check` before use as exception classes.
Converting Rust error values into a `PyErr` requires an explicit mutable
context.

### Concrete objects and conversions

Concrete types such as `PyBytes` and `PyModule` are transparent markers with
type-specific methods on `Bound<'py, Marker>`. The set grows with module
needs. `Bound<PyModule>` can add values and types.

`PyBaseException::new_exception_type` wraps `PyErr_NewException`, returning
`PyResult<'py, Bound<'py, PyType>>`. A custom metaclass can choose the C
function's unchecked result, so the wrapper uses
[`PyExceptionClass_Check`](https://docs.python.org/3/c-api/exceptions.html#c.PyExceptionClass_Check)
to require `BaseException` or a subclass, otherwise returning `TypeError`.
A type check alone is insufficient. Constructing a typed handle from an
unchecked C result is `unsafe`.

`PyBytes::new_with` lends a zeroed unpublished buffer; `new_with_uninit`
skips zeroing:

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

On error or panic the unpublished object is discarded unread. The closure
runs with the context suspended.

#### Conversions

```rust
pub trait FromPyObject<'a, 'py>: Sized {
    fn extract(
        ctx: &mut ThreadContext<'py>,
        obj: Borrowed<'a, 'py, PyAny>,
    ) -> PyResult<'py, Self>;
}

pub trait IntoPyObject<'py> {
    fn into_pyobject(
        self,
        ctx: &mut ThreadContext<'py>,
    ) -> PyResult<'py, Bound<'py, PyAny>>;
}
```

Both take exclusive access and are fallible, and implementations receive an
available context. `FromPyObject` keeps the owner lifetime `'a`, so borrowed
arguments cost no refcount; a reference `'view` into Python storage requires
`'a: 'view` and `'py: 'view`. The trait cannot return views that need
continuous attachment.

Exact, published `PyBytes` storage is immutable and safe to read while
detached, so its accessor is a leaf operation bounded by owner and entry
rather than by the context borrow:

```rust
impl<'a, 'py> Borrowed<'a, 'py, PyBytes> {
    pub fn as_bytes<'view>(
        &self,
        ctx: &ThreadContext<'py>,
    ) -> &'view [u8]
    where
        'a: 'view,
        'py: 'view;
}
```

It neither increfs nor copies:

```rust
let input = data.as_bytes(ctx);
let output = ctx.detach(|| compress_without_python(input))?;
```

This proof does not extend to arbitrary buffer exporters (see the
appendix).

Generic implementations compose:

```rust
let value: Option<u32> = FromPyObject::extract(ctx, argument)?;
let object: Bound<'py, PyAny> = value.into_pyobject(ctx)?;
```

Callbacks may return `T` or `PyResult<'py, T>`; only `PyResult` lets the
body raise, though converting `T` can still fail.

### Modules

Modules use PEP 793's `PyModExport` and `PySlot`. `export_module!` builds the
method table, ABI declaration, slot array, lifecycle callbacks, and export
symbol:

```rust
export_module! {
    name: _base64,
    doc: c"Base64 encoding implemented in Rust",
    state: Base64State,
    methods: [standard_b64encode],
    exec: base64_exec,
}
```

It binds `name` and `doc` to `const` items in safe code before the `unsafe`
slot construction. The slot, method, and ABI table types have private fields
and `unsafe` constructors that only the macro calls. `#[pyfunction]` emits a
`ModuleMethodDef<S>`, and `export_module!` accepts only methods with its
state `S`.

#### Definition identity

```rust
pub struct ModuleRef<'a, 'py, S> {
    module: Borrowed<'a, 'py, PyModule>,
    _state: PhantomData<fn() -> S>,
}
```

Only generated `ModuleDef<S>` glue constructs a `ModuleRef<S>`, ensuring a
valid state type and interpreter. Static definitions hold no
interpreter-owned objects; each module instance has its own state, types,
and exceptions.

The module token is its `ModuleDef<S>` static's address. A private
`UnsafeCell` field prevents const promotion and merging identical statics.
`ModuleDef<S>`, `ModuleRef<S>`, and `ModuleMethodDef<S>` are invariant in `S`
via `fn(S) -> S`, per the [safety rules](#safety-rules).

#### Module object access

Functions and methods may request an optional
`module: ModuleRef<'a, 'py, S>` parameter immediately after `mod_state`.
The macro recognizes its type and supplies the module used to obtain
`mod_state` (the function's module or the defining class's module); it is
not a Python argument. Module initialization already receives `ModuleRef`.

```rust
impl<'a, 'py, S> ModuleRef<'a, 'py, S> {
    pub fn as_object(&self) -> Borrowed<'a, 'py, PyModule>;
}
```

`module.as_object()` copies the stored borrowed handle without an incref;
its lifetime remains bounded by the module owner and entry scope. This
avoids storing a reference back to the module in its state.

#### Module state

Every module has typed state, even if empty:

```rust
pub trait ModuleState: Send + Sync + 'static + Sized {
    type Gc: ModuleStateGc<Self>;

    fn new<'a, 'py>(
        ctx: &mut ThreadContext<'py>,
        module: ModuleRef<'a, 'py, Self>,
    ) -> PyResult<'py, Self>;
}

// SAFETY: implementations must validate interpreter association, report every
// strong Python edge, and synchronize traversal, mutation, and partial clear.
pub unsafe trait ModuleStateGc<S: ModuleState>: 'static {
    const ENABLED: bool;

    fn validate<'py>(value: &S, ctx: &mut ThreadContext<'py>) -> PyResult<'py, ()>;
    fn traverse(value: &S, visit: &Visit<'_>) -> TraverseResult;
    fn clear(value: &S, ctx: &mut ThreadContext<'_>);
}

#[derive(ModuleStateGc)]
struct Base64State {
    error: GcSlot<PyType>,
}

impl ModuleState for Base64State {
    type Gc = Base64StateGc;

    fn new<'a, 'py>(
        ctx: &mut ThreadContext<'py>,
        _module: ModuleRef<'a, 'py, Self>,
    ) -> PyResult<'py, Self> {
        Ok(Self { error: GcSlot::empty(ctx) })
    }
}
```

Traced fields use `GcSlot<T>`, which owns an optional reference with an
interpreter ID fixed at construction. GC clear permanently closes a slot,
so reentrant code cannot repopulate it:

```rust
impl<T> GcSlot<T> {
    pub fn empty(ctx: &ThreadContext<'_>) -> Self;

    pub fn get<'py>(
        &self,
        ctx: &mut ThreadContext<'py>,
    ) -> Result<Option<Bound<'py, T>>, InterpreterMismatch>;

    pub fn replace<'py>(
        &self,
        ctx: &mut ThreadContext<'py>,
        value: Option<Bound<'py, T>>,
    ) -> Result<Option<Bound<'py, T>>, GcSlotReplaceError<'py, T>>;
}

pub struct GcSlotReplaceError<'py, T> {
    pub kind: GcSlotError,
    pub value: Option<Bound<'py, T>>,
}
```

`get` and `replace` take exclusive access because they may park, and check
the interpreter ID before touching the pointer. `get` increfs under
protection and returns `None` if cleared. `replace` returns the old
reference so it is released outside the lock; on mismatch or a closed slot,
it returns the incoming value unchanged inside the error. Replacing with
`None` empties without closing. Nothing exposes `&mut` to the stored handle.

#### Deriving GC policies

`#[derive(ModuleStateGc)]` and `#[derive(PyClassGc)]` classify fields by
their actual type through a sealed trait, since an alias can hide the
spelling:

- `GcSlot<T>` is traced.
- `Option`, `Box`, `Vec`, arrays, and boxed slices of slots are traced slot
  by slot; none can change through `&`. Spelling one directly is a macro
  error asking for direct fields.
- `Arc<GcSlot<T>>` and `Rc<GcSlot<T>>` are compile errors, even behind an
  alias, because shared ownership would double-report edges. The error
  fires after monomorphization.
- `Py<T>` is rejected by default, even inside a wrapper or behind an alias,
  because the collector cannot see it and a cycle through it would leak.
  Use `GcSlot<T>` instead, or mark a field `#[gc(untraced)]` to keep a
  reference that cannot be part of a cycle.
- Anything else is untraced; a slot hidden in another wrapper leaks its
  cycles, which is safe.

`ENABLED` comes from `core::mem::needs_drop` on the field types, because no
spelling proves a field holds no slot (even `u64` can be shadowed).
`GcSlot` has drop glue, so callbacks are omitted only when no field does.
Slots in `ManuallyDrop`, `MaybeUninit`, or unions escape the test but are
never released either, so the effect is a leak. Manual policies are `unsafe`
and must uphold the same rules.

#### GC slot synchronization

Clear uses synchronized removal through `&S`, never `&mut S` across a
decref. Traversal reads slots without locking, relying on this exclusion:

- Slot access requires attachment. A holder never parks, runs callbacks, or
  detaches while protected.
- Traversal, `gc.get_referents`, and `gc.get_referrers` run under the GIL or
  stop-the-world. `tp_clear` runs after the world restarts and uses the
  synchronized path.
- Debug `PyObject_GC_Track` can traverse without stopping the world, so
  factories keep the object exclusive until tracking returns.

Object traversal must not block, run Python, or hold an exclusive borrow
during a visit; lookups use the `_DuringGC` accessors. Clear releases
references only after the slot is closed and unlocked. External callers of
`tp_traverse` must provide the same exclusion.

#### State lifecycle

CPython creates the module and binds its functions before allocating state;
`PyModule_Exec` then allocates zeroed `StateStorage<S>` (interpreter ID,
atomic lifecycle state, `MaybeUninit<S>`). An unexecuted module is
reachable, from `importlib.util.module_from_spec` or `sys.modules`
mid-import, so every callback handles missing storage. The macro rejects
over-aligned types.

| Operation | Allowed source state | Published state and access |
| --- | --- | --- |
| Any callback before exec | No storage | Functions and methods raise `SystemError("module is not initialized")`; traverse, clear, and free do nothing. |
| Claim exec | `Uninitialized` | `Initializing`; any other source raises `SystemError`. |
| Finish `S::new` | `Initializing` | Validate traced associations before publication; on failure drop the unpublished value and publish `FailedWithoutValue`; otherwise write `S` and publish `Executing`. |
| Finish exec | `Executing` | `Ready` or `FailedWithValue`; `S` remains live. |
| Read state | `Ready` | Check storage identity; borrow `&S` for the intersection of the module-owner lifetime and guarded entry scope. |
| Traverse | `Executing`, `Ready`, `FailedWithValue`, `Clearing`, `Cleared`, `ClearFailed` | Borrow `&S`; never traverse `Dropping` or `Dropped`. |
| Clear | `Ready` or `FailedWithValue` | Enter `Clearing`, clear slots through `&S`, then publish `Cleared` or `ClearFailed`; later clears are no-ops. |
| Free with value | `Executing`, `Ready`, `FailedWithValue`, `Clearing`, `Cleared`, or `ClearFailed` | Enter `Dropping`, drop once, then publish `Dropped`. |
| Free without value | `Uninitialized` or `FailedWithoutValue` | Publish `Dropped` without calling `drop_in_place`. |

```rust
type ModuleExec<S> = for<'a, 'py> fn(
    &mut ThreadContext<'py>,
    ModuleRef<'a, 'py, S>,
    &S,
) -> PyResult<'py, ()>;
```

Publishing stores use release ordering and reads use acquire. Guards apply
failure states on unwind, and a panicking destructor is never retried.
Callbacks see state only once `Ready`, since exec may still be populating
it.

#### Support declarations

Generated modules declare `Py_MOD_GIL_NOT_USED`,
`Py_MOD_PER_INTERPRETER_GIL_SUPPORTED`, the internal exact-version ABI, and
the build mode. Foreign wrappers are supported through
`interop`, subject to their own safety contracts and the runtime's
availability and state-protection checks. Scoped worker threads are
permitted under the entry-lifetime rules; neither thread attachment nor
Python access may be inferred from a borrowed payload on a worker.
Process-global Rust caches may hold only Rust data or checked opaque
handles. The identity and lifetime checks apply even when import checks are
disabled.

### Argument parsing

```rust
#[pyfunction(signature = (data, value = 1, /))]
fn checksum<'a, 'py>(
    ctx: &mut ThreadContext<'py>,
    mod_state: &ChecksumState,
    data: Borrowed<'a, 'py, PyBytes>,
    value: u32,
) -> PyResult<'py, u32> {
    // ...
}
```

The wrapper supplies `ctx`, `mod_state`, and any optional `module` parameter;
only the remaining parameters appear in `signature` and are parsed from
Python arguments. Methods also receive `self` and the defining class, `cls`.

The macro checks names and emits a static `ParamSpec`. Values are extracted
with `FromPyObject`; defaults are Rust expressions. Vectorcall binding uses a
fixed stack array and allocation-free keyword matching, yielding
`Borrowed<'args, 'py, PyAny>` slots with no refcount churn. Constructor
tuple/dict parsing fills the same slots.

The glue checks its own inputs and keeps every argument alive while it is
in use:

- A `ParamSpec`/slot-count mismatch is a checked error.
- `kwnames` must be a tuple before its entries are read.
- Constructor keyword values are never borrowed from the `kwargs` dict:
  converting one argument can run Python code, such as `__index__`, that
  removes another value from the dict and frees it. Before converting any
  argument, the parser takes its own references to every keyword value and
  keeps them until the call and result conversion finish. It stores them in
  a private tuple rather than a list, because Python code can find a list
  through `gc.get_objects()` and clear it.
- Positional arguments and vectorcall arrays are borrowed without a copy:
  the caller keeps them alive for the whole call, and Python code cannot
  modify them.

Argument errors raise `TypeError`.

### Functions

Python-callable, module-scoped functions are ordinary Rust functions whose
first two parameters are `ctx` (the thread context) and `mod_state` (module
state):

```rust
/// Encode data using the standard Base64 alphabet.
#[pyfunction(signature = (data, /))]
fn standard_b64encode<'a, 'py>(
    ctx: &mut ThreadContext<'py>,
    _mod_state: &Base64State,
    data: Borrowed<'a, 'py, PyBytes>,
) -> PyResult<'py, Bound<'py, PyBytes>> {
    // ...
}
```

An optional `module` parameter provides
[module object access](#module-object-access).

A body using only leaf operations may take `&ThreadContext`. `#[pyfunction]`
emits a `METH_FASTCALL | METH_KEYWORDS` trampoline that enters panic
containment and `with_current`, validates the module against its
`ModuleDef<S>` and checks it has executed, binds arguments, calls the
function, and converts the result before `'py` ends. Methods use
`METH_METHOD`.

Argument binding, defaults, and the call live in a generated safe function,
so author code never compiles under `unsafe`. The macros reject `unsafe`,
`async`, `const`, `extern`, and generic functions.

`#[pyfunction]` and `#[pymethods]` use Rust doc comments (`///` or
`#[doc = ...]`) as the Python `__doc__`, stored in `ml_doc`.

#### Panic containment

The entry guard must outlive panic handling, so the catch sits inside
`with_current`:

```rust
// SAFETY: callers must obey PyCFunctionFastWithKeywords, provide live
// callback arguments, and invoke this function with an attached thread state.
unsafe extern "C" fn trampoline(
    /* existing PyCFunctionFastWithKeywords arguments */
) -> *mut ffi::PyObject {
    let boundary = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Author code runs in this safe closure, outside any unsafe block.
        let callback = |ctx: &mut ThreadContext<'_>| {
            let body = std::panic::catch_unwind(
                std::panic::AssertUnwindSafe(|| {
                    let value = call_rust_function(ctx);
                    convert_callback_result(ctx, value)
                }),
            );
            let result = match body {
                Ok(result) => result,
                Err(payload) => Err(panic_to_owned_error(ctx, payload)),
            };
            // The context and entry guard still protect cleanup and the
            // final transfer of an owned result/error to the C caller.
            finish_callback(ctx, result)
        };
        // SAFETY: callback inputs satisfy entry's attached-state contract.
        let entered = unsafe { with_current(callback) };
        match entered {
            Ok(ptr) => ptr,
            // SAFETY: this helper establishes private reporting authorization
            // or fails fatally; it never uses a rejected or stale context.
            Err(status) => unsafe { translate_entry_error_to_raw(status) },
        }
    }));
    match boundary {
        Ok(ptr) => ptr,
        // An entry/restoration/reporting invariant failed. Do not attempt to
        // report through a context whose guard may already have been dropped.
        Err(_) => std::process::abort(),
    }
}
```

`panic_to_owned_error` drops the payload with the context suspended, then builds a fixed-message
`SystemError`. Entry, restoration, and lifecycle guards never unwind; the
catch is a last-resort abort.

| Callback kind | Panic policy |
| --- | --- |
| Function, method, getter, setter, and constructor | Set a fixed `SystemError` and return the ABI's `NULL` or `-1` error sentinel. |
| Module create | Set a fixed `SystemError` and return `NULL`. |
| Module exec | Commit the appropriate failure state, set a fixed `SystemError`, and return `-1`. |
| Module or instance clear | Finish the lifecycle transition and set `SystemError`, which CPython reports as unraisable. Instance clear returns failure. Module clear returns 0, because CPython skips clearing the module dict when `m_clear` fails. |
| Traverse | Preserve incoming exception state and propagate the visitor's exact nonzero return and any error it sets. A Rust panic is fatal because continuing with an incomplete reachability report can violate GC invariants. |
| `tp_dealloc` and module free | Run mandatory non-panicking cleanup guards, then report the panic as unraisable under the [deallocator exception policy](#dropping). Abort if cleanup itself cannot be completed safely. |

Before dropping a payload or module state, deallocation commits the
destroyed state and installs cleanup guards, so a panic cannot cause a
double drop or skip cleanup. Destructors run with the context suspended and
receive no context. During finalization, suspension does not broaden what
the execution phase permits.

### Classes

A payload type `C` identifies a class's layout, static definition, module
state, and GC policy. Each module instance creates its own runtime type:

```rust
pub trait PyClass: Send + Sync + 'static + Sized {
    type State: ModuleState;
    type Gc: PyClassGc<Self>;

    fn definition() -> &'static ClassDef<Self>;
}

// SAFETY: implementations must validate interpreter association, report every
// strong Python edge, and synchronize traversal, mutation, and partial clear.
pub unsafe trait PyClassGc<C: PyClass>: 'static {
    const ENABLED: bool;

    fn validate<'py>(value: &C, ctx: &mut ThreadContext<'py>) -> PyResult<'py, ()>;
    fn traverse(value: &C, visit: &Visit<'_>) -> TraverseResult;
    fn clear(value: &C, ctx: &mut ThreadContext<'_>);
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
        .methods(Encoder::PY_METHODS)
        .getsets(Encoder::PY_GETSETS);
```

`#[derive(PyClassGc)]` uses the [module-state field rules](#deriving-gc-policies).
Every instance also owns an edge to its heap type, which generated traversal
reports even when `ENABLED` is false.

#### Class identity

Downcasts trust the address returned by `definition()` as the class token.
`ClassDef` uses the same private `UnsafeCell` field as `ModuleDef`. The
class marker trait is sealed. Generating the static through an `unsafe` or
sealed trait, as `export_module!` does, would also remove reliance on a safe
`definition()` method.

`ClassDef<C>`, `PyType<C>`, `PyInstance<C>`, generated method tables, and
`GcSlot<T>` are invariant. This prevents coercing a `Payload<fn(&str)>`
type object to `Payload<fn(&'static str)>`, constructing an instance, and
downcasting it back. Handles may be covariant in their marker because each
parameterized marker is invariant.

`PyType<C = UntypedClass>` and `PyInstance<C>` are transparent markers.
Upcasts to `PyType` or `PyAny` are free; downcasts check the context, then
the class token.

#### Types and instances

`ClassDef<C>::create(ctx, ModuleRef<C::State>)` returns
`Bound<PyType<C>>`. Each `C` has exactly one definition; a second needs a
Rust newtype. The definition address is also the `Py_tp_token`, which
proves layout, not interpreter identity: several module instances can have
types with the same token. There is no global type cache.

`type_object.new_instance(ctx, payload)` validates the token and every
payload slot's interpreter ID before publishing. It allocates with
`_PyType_AllocNoTrack` (`PyType_GenericAlloc` tracks too early), writes the
payload and lifecycle metadata, then calls `PyObject_GC_Track`. On failure or
unwind, the unpublished payload is dropped with the context suspended.

Methods recover the defining module through `METH_METHOD`, validate its
state and interpreter ID, and bound borrows by owner and entry (see
[Methods](#methods)). `&self` borrows the instance's Rust payload;
concurrent calls on free-threaded builds require interior mutability rather
than `&mut self`.

```rust
#[derive(PyClassGc)]
struct Node {
    parent: GcSlot<PyInstance<Self>>,
    value: u64,
}
```

All generated instances set `Py_TPFLAGS_HAVE_GC` and traverse their type
edge; `Node` also traverses and clears its payload edge:

| Operation | Invariant |
| --- | --- |
| Allocate | Validate payload interpreter IDs against the type/context ID; write `C` into an untracked object, publish ready, then call `PyObject_GC_Track`. |
| Traverse | Visit the actual heap type and payload edges through shared access; only call `visitproc`, propagate its exact return, and never invoke Python or block independently of the visitor. |
| Clear | Remove payload edges through synchronized `GcSlot` access using `&C`; release removed references after leaving slot protection; later clears are no-ops. The instance retains its type until deallocation. |
| Deallocate | Untrack first. Contain any required clear independently; then commit dropping, drop `C` once, run allocation/type guards, and report the first panic. |

Types are immutable, final (preserving the `METH_METHOD` defining-class
link), and non-instantiable unless they define `tp_new`.

#### Methods

Instance methods take `&self`, `cls`, `ctx`, and `mod_state`, followed by
their Python arguments:

```rust
#[pymethods]
impl Encoder {
    #[pyfunction(signature = (data, /))]
    fn encode<'a, 'py>(
        &self,
        cls: Borrowed<'a, 'py, PyType<Self>>,
        ctx: &mut ThreadContext<'py>,
        mod_state: &ModuleStateType,
        data: Borrowed<'a, 'py, PyBytes>,
    ) -> PyResult<'py, Bound<'py, PyBytes>> {
        // ...
    }
}
```

`cls` is the defining class supplied by `METH_METHOD`; `mod_state` is a
shared borrow of that class's module state (`&C::State`). The wrapper
supplies these parameters before parsing Python arguments.

`#[pymethods]` accepts:

- `#[pyfunction]`: an instance method with a `PyCMethod` trampoline;
- `#[getter]` and `#[setter]`: halves of a `ClassGetSetDef<C>`, using the
  same `&self`, `cls`, `ctx`, and `mod_state` prefix. Their glue obtains
  `cls` from the instance's type, since generated types are final;
- `#[new]`: a `tp_new` trampoline taking `cls`, `ctx`, and `mod_state`
  before Python arguments. It constructs `C` and moves it into a new
  allocation.

Setters reject `NULL` (attribute deletion) with `AttributeError` and `-1`
before conversion or author code; Python `None` is an ordinary value. A
future `#[deleter]` could share the C `PyGetSetDef.set` slot.

`#[pymethods]` emits `C::PY_METHODS`, `C::PY_GETSETS`, and `C::PY_NEW`,
installed with `methods`, `getsets`, and `constructor`. All function-glue
rules apply.

## Testing

CI runs `cargo fmt` and [Clippy](https://github.com/rust-lang/rust-clippy),
checks context or sealed capabilities on Python-touching safe wrappers, and
audits shared wrappers as leaf operations. Both release and debug builds
exercise the [transition checks](#transition-checks).

A new `_testrustapi` module in the regular test suite covers the API's
correctness. Entry tests cover already-attached and native callers,
restoration of the caller's prior attachment, cleanup authorization, and
balanced token/guard release on success, setup failure, and panic.

## Appendix: Open decisions and future work

- **Upstream CPython prerequisites.** These bugs reproduce without Rust:
  1. Module-state allocation publishes `md_state` before zeroing it. Concurrent
    callbacks need initialized storage and synchronized pointer publication
    (see [State lifecycle](#state-lifecycle)).
  2. A PEP 788 guard held by a thread that does not survive `fork()` stays
    counted, hanging the child's finalization. Public guards can be handed
    between threads, so a guard from a vanished thread may belong to the
    survivor. Guards need an ownership or fork-invalidation contract.
  3. A daemon thread blocked in a subinterpreter ended by main-interpreter
    finalization can keep a dangling interpreter pointer and read freed
    memory on reattach. This design inherits the bug and does not fix it.
  4. A guard holder waiting on a pending call scheduled after the finalizer's
    last `_Py_FinishPendingCalls` deadlocks. Scheduling must wake
    finalizers, and the finalizer must run pending calls before parking.
- **Static builds.** Static builds register modules through `PyInit_*`
  inittab entries, so `PyModExport_<name>` needs inittab support.
- **Reclamation after shutdown.** Revocable ownership records and reserved
  storage could release references that are currently abandoned, at a cost
  to every handle.
- **Buffer protocol.** A safe `PyBuffer` must distinguish read-only and
  writable exporters, specify contiguity, format, alignment, ownership, and
  release, and prevent slices from racing Python mutation or escaping across
  detach.
- **Locking.** A lock that cooperates with attachment, the GIL, and
  stop-the-world is needed, and could be modeled after [PyO3's `PyOnceLock`](https://github.com/PyO3/pyo3/blob/4045f8d92045fbf55438eb6cae175b853c5835ce/src/sync/once_lock.rs#L42).
- **Guard performance.** If measurements justify avoiding the public
  guards' allocation or shared-counter contention, a private optimization
  must preserve whole-entry lifetime and admission guarantees.
  Per-thread-state counters require atomic acquisition while attached, no
  safepoint between the admission check and increment, and a synchronized
  final recheck of every counter under stop-the-world or the GIL. Fork interaction and
  finalizer wakeups must be proved again; an optional guard around explicit
  detaches is not an equivalent optimization.
- **Shutdown progress.** Whole-entry guards allow blocked daemon threads to
  delay finalization indefinitely. A future nonblocking design would need
  a complete ownership proof for every allocation backing an exposed Rust
  reference, including ancestor entries and deferred-counted instances.
  Pinning only the current entry's module does not establish that proof.
- **Mutable GC containers.** Caches, callback registries, and `_asyncio`
  task sets need mutable collections of traced references. Current payloads
  expose only `&self`, and containers of `GcSlot<T>` cannot change through
  `&`. An untraced `Mutex` permits mutation but leaks cycles. Future
  `GcVec<T>` or `GcMap<K, T>` containers must preserve these invariants:
  - every reference is checked against the container's interpreter ID
    when it is inserted, not at GC time;
  - traversal reports each strong reference exactly once;
  - traversal reads without locking, so no thread may be paused
    mid-mutation while holding the container's lock;
  - clear closes the container, so finalizers cannot re-add edges, and
    releases references only after the lock is dropped.

  `Mutex<Vec<Py<T>>>` fails the third requirement: traversal can neither
  lock a mutex held by a paused thread nor read a buffer that a concurrent
  `push` may reallocate.
- **Borrowed `GcSlot` reads.** `get` increfs because another thread may
  replace and decref the value. A `Borrowed` result would need a guard that
  blocks replacement and permits only leaf operations, perhaps by borrowing
  `&ThreadContext` throughout. Whether a reader may pause on a contended
  lock while holding this guard remains open.
- **Explicit thread state C APIs** and inlining the entry counters could
  speed hot paths without changing the Rust API.
- **Panic strategy.** The API requires `std` and `panic=unwind`. A
  `panic=abort` build is ABI-safe but cannot recover; a `no_std` build would
  need panic-abort or another no-unwind boundary.
