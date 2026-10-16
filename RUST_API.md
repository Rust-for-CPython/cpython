# Rust for CPython: API Design for 3.16

## Overview

This document designs a safe Rust API for in-tree CPython extension modules. `zlib` is the first full migration target.

Rust can call the C API efficiently. However, raw FFI does not encode Rust's
lifetime, aliasing, or thread-safety rules. The wrapper described here
restores them. The design depends on new private CPython support (a per-thread
availability flag, per-thread-state protection and lease counts, transition
checks, and a release queue that accepts references without an attached
thread state), so a modified runtime is needed for safety. Several existing
CPython bugs need fixes; the appendix lists them as upstream prerequisites.

## Glossary

This glossary explains the design-specific uses of terms in this document.
Standard Rust terms use their usual meanings; in particular,
`&T` is a shared borrow and `&mut T` is an exclusive (mutable) borrow.

- **Entry:** a call into Rust from Python or the C API, such as a module
  function, method, or slot callback (`exec`, `clear`, etc.). Generated
  **entry glue** wraps the Rust function; its private **entry guard** keeps
  resources alive through argument parsing, the call, result conversion,
  and cleanup. See [Entry glue](#entry-glue).
- **Execution phase:** the circumstances in which CPython calls Rust:
  ordinary execution, finalization cleanup, or GC traversal. Each permits
  different operations; see [Execution phases](#execution-phases).
- **Capability:** a value or reference that proves permission to perform an
  operation. Here this is normally a borrow of `ThreadContext`, with
  restricted alternatives for GC traversal and reference release.
- **Class payload:** the Rust data stored in a Python instance. See
  [Classes](#classes).
- **Leaf operation:** an operation that preserves attachment and
  cannot re-enter Python, invoke arbitrary Rust callbacks, run deallocators,
  or reach a safepoint that suspends execution or permits re-entry. It may
  still call other C functions. Examples include binding a handle to a
  context and reading immutable bytes; see
  [The thread context](#the-thread-context).
- **Safepoint:** a point in a runtime operation where the thread may pause
  or service pending work, for example to handle a stop-the-world request.
- **Exclusive context access:** an `&mut ThreadContext` borrow. Operations
  that can run callbacks or change attachment require it, and temporarily
  suspend the context while making the call. This is Rust's exclusive
  borrow, not a lock on the interpreter or its objects.
- **Available / suspended context:** an available context may be used by
  safe Rust code. Suspension temporarily makes it unavailable while keeping
  the original thread state protected. Suspension alone does not detach
  the thread; see [Scope transitions](#scope-transitions).
- **Finalization lease:** keeps an interpreter alive for the duration of a
  Rust entry, including while detached. See
  [Finalization leases](#finalization-leases).
- **Admission:** permission to start an entry or submit a reference for
  release. Closing admission during finalization prevents new work, apart
  from the explicitly authorized cleanup described below.
- **Gate:** a runtime check that rejects a thread-state transition, such as
  attachment, detachment, or deletion, when it would invalidate a Rust
  context. See [Gated and exempt paths](#gated-and-exempt-paths).
- **Release sink / queue:** a sink buffers references awaiting `Py_DECREF`
  for one entry; the queue holds them for an interpreter. **Draining** means
  performing those deferred decrefs. See
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
9. [Functions](#functions)
10. [Classes](#classes)
11. [Argument parsing](#argument-parsing)

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
may touch Python requires a capability: a borrow of `ThreadContext` (shared
for audited leaf operations, exclusive for anything that can run callbacks
or change attachment), or a restricted private capability for GC traversal
or reference release. An operation without one needs an audited proof that
its whole call path does not access a thread state. Until there is async
`Drop`/`Drop` with context, `Drop` never obtains a capability; it only submits
a release.

### Interpreter identity and lifetime

#### Interpreter keys

Each interpreter lifetime has a private `InterpreterKey`: a nonzero 64-bit
generation number from a process-wide counter that survives `Py_FinalizeEx`
and the runtime reset in `_PyRuntimeState_Init`. Every interpreter,
including a main interpreter created after reinitialization, gets a fresh
key; exhaustion fails interpreter creation. Public interpreter IDs cannot
serve this role, because `_PyInterpreterState_Enable` resets `next_id` and
the main interpreter is always ID zero. For the same reason an interpreter
view, which resolves by ID, cannot prove generation identity.

A key names the interpreter under whose API contract a reference was
acquired, not the one that allocated the object. An accessor in interpreter
B may return a B-associated handle to a process-global constant, but an
existing A-associated handle is never relabeled B because the object is
immortal or the pointers are equal.

Objects carry no interpreter association. Generated glue reads the key from the attached thread state's
interpreter, with no allocation, search, or lock, and conversions preserve
it.

#### Finalization leases

A key does not keep its interpreter alive. Each Rust entry therefore holds a
**lease** for its whole duration, including while detached, and
finalization waits until no lease remains.

- Nested entries take their own leases. While an enclosing entry holds one,
  admission cannot close, so nested acquisition succeeds.
- Entry glue releases the lease last, after scoped values are consumed,
  queued releases are drained or transferred, and panic reporting finishes.
- A stored `Py<T>` never holds a lease, or a `Py<T>` in module state would
  block the shutdown that destroys it.

The lease must cover the whole entry, not just explicit detaches. Attachment
protects the entry thread itself, since interpreter finalization hangs a paused thread. Attachment does not protect borrows the entry has lent out. `&S`, `&C`, and `&[u8]` point to
`Sync` data, so they can reach scoped worker threads:

1. An entry on a daemon thread lends `&S` to a scoped worker.
2. The entry calls Python, which detaches internally (e.g. `socket.recv`).
3. Finalization hangs the daemon thread on reattach and removes its state.
4. The worker is still reading `S`.

On free-threaded builds, modules and module-level functions use deferred
reference counting, and their stack references are roots only through
registered thread states. After step 3 the module and `S` can be freed
under the worker. The whole-entry lease also keeps the entry's release sink
valid until drained.

The cost is that an entry blocked indefinitely in a Python callback blocks
shutdown indefinitely (see *Shutdown progress* in the appendix). This is
deliberate.

Two fatal checks reject direct lease-dependent shutdown cycles:

- **Finalizing while holding a lease.** `Py_EndInterpreter` and
  `Py_FinalizeEx` fail fatally at the start of finalization if the calling OS
  thread holds any lease, across all its thread states and interpreters.
  Otherwise two threads could hold leases on A and B while each finalizes
  the other.
  Finalization scans the existing counts under the runtime's thread-list
  lock; this adds no bookkeeping to ordinary entry or exit.
- **Hanging a leased thread.** A thread may hold a lease on A while running
  in B (e.g. via `interp.exec()`). If B's finalization hung it, A could
  never finish finalizing. CPython's hang paths, including the one in
  `PyGILState_Ensure`, therefore fail fatally when the thread holds any
  lease. Direct calls to the public `PyThread_hang_thread` bypass this. A
  later refinement could make B wait for such threads instead.

#### Lease protocol

Leases build on [PEP 788 interpreter guards](https://peps.python.org/pep-0788/).
Acquisition synchronizes with finalization's admission barrier; once
admission closes, entry fails before exposing any Rust reference. Lease
bookkeeping lives in the entry guard on the stack, so entry allocates
nothing.

Leases do not use the guards' shared `finalization_guards` counter, which
would make every entry write one shared cache line. Each thread state counts
its own leases:

- **Acquire:** read the interpreter's admission-closed flag, then increment
  the attached state's lease count, atomically and with no safepoint
  between.
- **Release:** decrement the count. If it reaches zero during finalization,
  [wake the finalizer](#waking-the-finalizer) and do not touch the
  interpreter afterwards.
- **Wait:** admission stays open so lease holders can make nested callbacks.
  The finalizer sums the counts under the runtime's thread-list lock and
  parks until woken.
- **Close:** once the sum is zero, stop the world (or hold the GIL), recheck
  every count, the guard counter, and the release queue, and set the flag
  only if all are clear; otherwise restart and wait again.
- A state with a nonzero lease count is protected from deletion.

Lease acquisition requires attachment to the target interpreter throughout
the admission check and counter increment. No safepoint, detachment, or
reentrant call may occur between them. Closing admission requires a
completed stop-the-world handshake, or ownership of that interpreter's GIL,
held continuously through the final recheck and closure. The synchronization
must make preceding lease increments visible to the finalizer and admission
closure visible to subsequent entrants. Merely requesting a stop is
insufficient.

PEP 788 C guards keep their shared counter, because a guard can be created
without an attached thread state and closed on another thread.

#### Cleanup authorization

Callbacks CPython invokes during finalization, and synchronous callbacks
they cause, receive **cleanup authorization** instead of a lease. After
admission closes, an entry is authorized if and only if its attached thread
state is the one finalizing that interpreter. Cleanup contexts cannot
request explicit detachment or unrestricted interoperation; an audited C
call may detach internally only if finalization cannot advance until it
returns. User code and other threads cannot manufacture authorization. It
ends at the [final drain point](#shutdown).

#### Future entry from threads created outside Python

The initial API creates contexts only at generated C entries. A future safe
API for threads created outside Python should build on
`PyThreadState_Ensure` and `PyThreadState_EnsureFromView`, take an explicit
interpreter key rather than trusting view lookup, acquire a lease before
attaching, and report shutdown
failure without a Python exception. `PyGILState_Ensure` must not implicitly
select the main interpreter.

### The thread context

`ThreadContext<'py>` is a scoped, exclusive capability for one exact thread
state on the current OS thread. Whenever safe code can use it, that state is
attached and protected against finalization.

```rust
pub struct ThreadContext<'py> {
    ptr: NonNull<ffi::PyThreadState>,
    interpreter: InterpreterKey,
    entry: NonNull<EntryState>, // private, stable storage in the entry guard
    #[allow(clippy::type_complexity)]
    _marker: PhantomData<(fn(&'py ()) -> &'py (), *mut ())>,
}
```

The marker makes `'py` invariant and the context `!Send + !Sync`. It is
neither `Copy` nor `Clone`, and only generated entry glue constructs it.
`'py` is a fresh lifetime per entry, and `Bound` and `Borrowed` are invariant in
it too, so separate entries cannot mix scoped values even within one
interpreter.

| Context argument | Permitted operations | Examples |
| --- | --- | --- |
| `&ThreadContext<'py>` | Audited leaf operations that preserve attachment and cannot invoke arbitrary Rust or Python callbacks, run deallocators, or reach a suspending/reentrant safepoint. | Checked binding, ownership-preserving casts, exact immutable bytes access, and an audited incref. |
| `&mut ThreadContext<'py>` | Operations that may execute Python, call user code, allocate through a potentially reentrant path, decref, drain releases, detach, or switch states temporarily. | Generic extraction, object creation, attribute access, explicit destruction, and `detach`. |

The classification covers the entire call path, including failure handling,
allocator hooks, and cleanup. A shared-context operation reports failures
as Rust error values without setting Python's error indicator, since raising
would need exclusive access. New wrappers default to exclusive until audited,
and a blocking acquisition is never a leaf operation. Attachment does not
protect object contents from concurrent mutation on free-threaded builds.

Holding `&mut ThreadContext` does not make the context unavailable. A
private wrapper **suspends** it only around the actual reentrant C call or
closure, and never passes a suspended context to author code.

#### Execution phases

An **execution phase** determines which operations CPython permits when it
calls Rust. It is separate from whether a thread is attached or a context
is available or suspended.

| Execution phase | Permitted operations |
| --- | --- |
| Ordinary execution | An entry holds a finalization lease and may use the operations allowed by its shared or exclusive context borrow, including explicit detachment with exclusive access. |
| Finalization cleanup | Callbacks use [cleanup authorization](#cleanup-authorization). They cannot request explicit detachment or unrestricted interoperation; an audited C call may detach internally only if finalization cannot advance until it returns. |
| GC traversal | Callbacks use the restricted GC visitor operations, without a general-purpose `ThreadContext`. They must obey the [GC slot synchronization](#gc-slot-synchronization) rules. |

After the [final drain point](#shutdown), no entry can obtain a context.
The module and instance lifecycle states described later (such as `Ready`
and `Cleared`) track initialization and destruction separately.

#### Availability flag and protection count

Borrowing alone cannot stop another safe Rust library from calling
`PyEval_SaveThread`, and a detached closure can reach scoped TLS without
crossing a generated entry. CPython therefore adds:

- **An availability flag** per OS thread, recording whether a Rust context
  is available there. It is shared by all extensions and interpreters on
  the thread. At most one context is available per thread, and entry and
  restoration guards are lexically scoped, so one flag suffices and no scope
  stack is needed. It lives in CPython because each extension links its own
  copy of the Rust support crate. Only the owning thread touches it, so it
  needs no atomics, and placed beside the current-state pointer it makes the
  common gate one load and branch.
- **A protection count** per `PyThreadState`, incremented at entry and
  decremented at exit, so a suspended entry still protects its state.
  Release consumers do the same while holding a batch. It is atomic because
  deletion can run on another thread.

#### Scope transitions

1. **Enter:** require the flag clear; verify the attached state and
   [execution phase](#execution-phases); acquire a lease (or use cleanup
   authorization during finalization); increment the protection count. Only
   then set the flag and publish the context.
2. **Suspend:** clear the flag before a reentrant operation. Refusal by the
   initial checks leaves attachment and the flag unchanged without raising.
   The restoration guard keeps the original state. Suspension does not
   broaden the C operations the execution phase permits.
3. **Gate:** CPython's attach, detach, swap, and deletion paths, public and
   internal, reject any transition while the flag is set, even with no state
   attached. Deletion also rejects a state with a nonzero protection count,
   even from another thread.
4. **Resume:** require the flag clear and the original state attached, apply
   the [exception policy](#exceptions), then set the flag. If a foreign call
   failed to restore the state, fail fatally rather than attach over an
   unknown state.
5. **Exit:** clear the flag, service or transfer the entry's releases, and
   retire the sink once empty. Cleanup and panic reporting precede releasing
   the protection count and lease.

Entry and restoration guards are private and cannot be forgotten by safe
code, which the single flag depends on. Forgetting a public handle may leak
a reference but cannot affect the flag or protection. The entry guard owns
the release sink at a stable stack address.

Rust wrappers check permission before calling `PyEval_SaveThread`. Legacy C
transitions that bypass the protocol while the flag is set fail fatally
before changing attachment: continuing would break safe Rust's proof, and
waiting would self-deadlock.

#### Gated and exempt paths

- **Gated:** the internal detach path (reached by every public release and
  swap), deletion of the current thread state, `Py_MakePendingCalls`, and the
  deprecated `PyEval_ReleaseLock`. Clearing or deleting any state checks its
  protection count.
- **Exempt:** `_PyParkingLot_Park`, through a
  private thread-local marker, and the `_PyThreadState_Suspend`
  stop-the-world safepoint.

A path is exempt only if no callback or Rust access can run before the
exact state is restored. The parking lot calls the public
`PyEval_ReleaseThread`/`PyEval_AcquireThread`, so the private marker is what
distinguishes it from explicit detachment. Lock paths that call
`Py_MakePendingCalls` for signals require exclusive context access and
suspension instead.

The gate does not stop foreign code from running Python on the attached
state (`PyObject_Call` from another library, or `PyErr_CheckSignals`). Such
use need not violate the foreign library's contract. Every Rust boundary it
can reach checks the flag: a nested entry or gated detach fails fatally, GC
clear of a Rust object is refused (leaking the cycle), and payloads are
dropped without a context as always. Foreign code can also set an exception
without a transition; wrappers handle this through the [exception
policy](#exceptions). Transition hooks also affect non-Rust code and must
be benchmarked.

#### Entry glue
The following is a simplified example of what the Rust entrypoint glue for a function call looks like:

```rust
// SAFETY: only generated C callbacks with an attached state and valid inputs
// may enter; the implementation checks the execution phase and establishes
// a context with a lease or cleanup authorization.
unsafe fn with_current<R>(
    f: impl for<'py> FnOnce(&mut ThreadContext<'py>) -> R,
) -> Result<R, EntryError>;
```

`for<'py>` makes the lifetime fresh, so `R` cannot depend on it, and macros
reject author functions that name a specific lifetime such as `'static`.
Generated wrappers convert results to raw C values before
returning. The entry guard lives through parsing, conversion, unwinding,
release processing, and exception reporting. `EntryError` is a Rust error
value; callback-specific glue reports it without manufacturing a context
after admission closes.

#### Detachment and foreign wrappers

```rust
impl<'py> ThreadContext<'py> {
    pub fn detach<R>(
        &mut self,
        f: impl FnOnce() -> R,
    ) -> Result<R, DetachError>;

    pub fn detach_if_possible<R>(
        &mut self,
        f: impl FnOnce() -> R,
    ) -> PyResult<R>;

    pub fn interop<R>(
        &mut self,
        f: impl FnOnce() -> R,
    ) -> Result<R, InteropError>;

    pub fn drain_releases(&mut self);
}
```

- **`detach`** suspends the context, detaches, runs `f` on the same thread,
  and restores the state, including on panic. Refusal returns `DetachError`
  before detaching. Cleanup entries refuse it. Nested callbacks may detach.
  Pending exceptions use `DetachError::Exception(PyErr)`.
- **`detach_if_possible`** detaches when the execution phase permits and
  otherwise runs `f` attached, after ensuring a clean exception state. Pending
  exceptions return `Err(PyErr)`. Use it only for work valid while attached. Code that waits on another thread needing
  Python must use strict `detach`, because attached waits can deadlock
  through the GIL or stop-the-world.
- **`interop`** suspends without detaching, so `f` can use a foreign safe
  wrapper that manages its own attachment, including switching
  interpreters. The boundary verifies the incoming state was restored. If
  `f` leaves an exception pending, `interop` takes it and returns
  `InteropError::Exception(PyErr)` instead of `f`'s result, so the caller
  can propagate or handle it. Cleanup entries refuse it. It does not repair
  a foreign wrapper's own soundness.
- **`drain_releases`** services queued releases (see [Draining](#draining)).

The three closure APIs return any preexisting exception through their error
channel without running `f`. On return they capture pending exceptions
before restoring availability, following the [exception policy](#exceptions).

The closures receive no context and need no `Send` bounds: they run on the
same thread, and the proof is exclusive access to the capability. Handles
may move in and out but remain unusable without an available context.

This closes the holes in PyO3
[#2141](https://github.com/PyO3/pyo3/issues/2141) and
[#3640](https://github.com/PyO3/pyo3/issues/3640), where `SendWrapper` or
scoped TLS let code reach Python without proof of attachment. Here a shared
context reference stashed in TLS blocks the exclusive borrow that detaching
needs, a `RefCell` holding the context fails a reentrant borrow, and
foreign detachment hits the CPython gate. Likewise a TLS hook running while a
C call has switched to interpreter B cannot use A's context, which stays
exclusively borrowed.

No implicit operation (`Debug`, equality, hashing, indexing, iteration,
`Drop`) touches Python.

### Python object handles

| Handle | Owns a reference | Lifetime | Auto traits | Drop |
| --- | --- | --- | --- | --- |
| `Borrowed<'a, 'py, T>` | No | owner `'a` and attachment `'py` | `Copy`, `!Send`, `!Sync` | none |
| `Bound<'py, T>` | Yes | guarded entry `'py` | `!Send`, `!Sync` | enqueue in entry release sink |
| `Py<T>` | Yes | unbound, for stored state | `Send`, `Sync` | enqueue by interpreter key |

`Borrowed` stores only a pointer; its owner supplies `'a` and the context
supplies `'py`. `Bound` stores the pointer and a pointer to its entry's
release sink, which records the interpreter key. `Py<T>` stores the pointer
and an `InterpreterKey`. Both owning forms are two words on 64-bit targets.
The entry's `'py` lifetime proves association, not permission: handles can
exist while the context is suspended but cannot be used without an available
context.

#### Binding

Every safe `Py<T>` operation that touches the object (binding, downcasting,
cloning, conversion, destruction) first compares its key with the
context's, before dereferencing anything. Mismatch returns the Rust error
`InterpreterMismatch` without setting Python's error indicator. This also
rejects expired handles whose address a later interpreter reuses.

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
unbinding, and transfers preserve the key.

#### References into Python memory

Rust references into Python-owned memory (byte slices, class payloads,
module state) are bounded by both the owner's lifetime and the entry scope.
A static `Py<T>` cannot yield a `'static` `&[u8]`, `&C`, or `&S`, and the
entry cannot return such references. A view that needs continuous
attachment also borrows the context.

Attachment is not a lock: other threads and foreign wrappers can mutate
Python objects. Neither the gate nor `&ThreadContext` justifies a slice into
mutable storage, an unsynchronized `&Cell<_>` payload, or `&mut` into
published memory. Shared class and module payloads must be `Send + Sync`.

#### Dropping

```rust
object.drop_with(ctx)?;
```

`drop_with(&mut ctx)` releases directly with exclusive context access and
the context suspended during the decref.
For `Py`, it returns `InterpreterMismatch` on mismatch after falling back to
an ordinary queued drop.

Ordinary `Drop` never decrefs, attaches, or switches states, even on the
right interpreter: a finalizer could reenter Python while a shared context is
reachable through TLS. `Bound::drop` appends to its function entry's sink;
`Py::drop` submits to its interpreter's queue. Neither reads the object.
Sealed markers for immortal singletons (`None`, booleans) may skip release
entirely; a general integer marker may not.

`Drop` must not unwind. Explicit release and draining preserve the incoming
exception. Like any deallocator, generated `tp_dealloc` and module free must
leave the exception state exactly as they found it; CPython debug builds abort
otherwise. They save and clear any pending exception before cleanup starts,
and restore exactly what they saved, including no exception, on every exit
path. A panic, or an exception left behind by cleanup, is reported as
unraisable before the restore.

#### Markers and GC

`PyAny` is the marker for any Python object; specific markers like `PyBytes`
share its transparent layout, so upcasts are free. There is no `&mut PyAny`,
since Python objects can change through aliases.

A `Py<T>` in pure Rust state may stay opaque after its interpreter expires.
Publishing it as a traced GC edge requires association with the containing
object's interpreter at construction and every mutation.

### Deferred reference release

#### Entry sinks

Each entry has a release sink with a small inline buffer, owned by the entry
guard. `Bound::drop` appends to it with no allocation, lock, or Python call.
When it fills, it transfers a batch to the interpreter queue or obtains
overflow storage without calling Python or attaching a thread state, never
decrefing in `Drop`. If both fail, only the incoming reference is abandoned;
accepted ones are kept.

**Aliasing invariant:** no `&mut` to sink storage may be live across an
operation that can drop a handle into that sink. Pushes use interior
mutability, and draining extracts a batch before decrefing.

There is no thread-wide sink lookup: `Py::drop` always uses the interpreter
queue, even inside a same-interpreter entry. Stored handles are mostly
dropped at teardown, and `drop_with` avoids the queue when promptness
matters. Exit drains or transfers every buffered reference before retiring
the sink, draining locally if transfer fails. After admission closes,
authorized cleanup sinks overflow through a private cleanup route.

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
    _PyInterpreterKey interpreter,
    PyObject *owned_reference);
```

Only `_Py_RELEASE_QUEUED` consumes the reference: the queue takes it over
without an incref, and the producer must not also decref it. Failure
consumes nothing, raises nothing, and does not inspect the object.

Submission works from any OS thread (detached, in another interpreter,
or after `Py_FinalizeEx`). It must not read the object, touch its refcount,
call Python, attach, or panic, and it must not wait for the target
interpreter. Allocation must not call Python or attach a thread state:
`PyMem_RawMalloc` does not qualify, because under tracemalloc its hook calls
`PyGILState_Ensure`. C `malloc` and an OS mutex do.

A process-lifetime registry maps keys to queue endpoints and survives
reinitialization. Lookup, admission, and notification are protected against
interpreter deletion as one protocol. Retirement removes the endpoint before
freeing the interpreter and waits for in-flight producers, so a missing key
means expired and never aliases a new interpreter.

#### Notification

Notification must not depend on the bounded pending-call array, and all
eight eval-breaker bits are taken. A producer sets a per-interpreter
`releases_pending` flag and, on the clear-to-set edge, the existing
calls-to-do bit. GIL builds also check the flag on GIL acquisition. Producers
attached to the target set their own eval breaker; others signal its threads
under the thread-list lock.

Drains during eval-breaker handling or GIL acquisition are bounded by node
count so producers cannot starve the eval loop. A drain that leaves work, or
defers because a context is available, re-arms the flag.

#### Waking the finalizer

A finalizer parked waiting for guards and leases needs a wakeup that does
not require attachment, and whoever releases the last guard or lease must
not touch the interpreter, which may be freed at once. The wake word and a waiting-finalizer count
therefore live in process-global storage shared by all finalizers; wakeups
for other interpreters are harmless. Wake events are a producer setting
`releases_pending`, release of the last Rust lease or C guard, and the
in-flight submission count reaching zero. After releasing its final
lifetime protection, a producer accesses only process-global notification
storage.

A waker does nothing if the waiter count is zero; otherwise it increments the
word and unparks **all** waiters (`_PyParkingLot_UnparkAll`). A single wake
could select another interpreter's finalizer and leave the ready one asleep.
The finalizer registers as a waiter first, then on each iteration reads the
word, services releases, checks the counts, and parks expecting the word it
read. With sequentially consistent atomics no wakeup is lost. The loop should
also run pending calls before parking, and scheduling a
pending call should wake finalizers; otherwise a guard holder waiting on a
pending call scheduled after the last `_Py_FinishPendingCalls` deadlocks (an
existing bug; see the appendix).

#### Draining

Draining requires exclusive context access and suspension via
`ctx.drain_releases()`, or private release authorization between scopes.
Eval-breaker handling and GIL acquisition defer draining while any context
on the thread is available.

A consumer that finds work takes one lease and increments its state's
protection count for the batch, holding them through decrefs and reentrant
callbacks, so shutdown accounts for in-progress batches. It takes no lease
for an empty queue, since releasing one would wake the finalizer forever. The
consumer releases locks that do not require attachment before decrefing.
Each reference is released exactly once, attached to its own interpreter;
until then it counts as an
external strong reference for GC, so moving an edge out of a GC field
removes the field's edge before submitting the release.

Deferred release promises neither destruction before `Drop` returns nor on
the dropping thread. Resources needing prompt release should expose an
explicit close method.

#### Shutdown

1. **Open:** the queue accepts and drains normally.
2. **Pre-finalization:** the finalizer lets leased entries and consumers
   finish while the interpreter is intact, servicing releases and releasing
   its GIL and locks while it waits.
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
leak but never exposes an expired object. Since safe Rust can forget handles,
correctness never depends on
destructors running. Reclaiming every retained `Py<T>` would need revocable
ownership records on every handle (see the appendix).

#### Fork

Fork support with active Rust entries is still TBD.

### Exceptions

```rust
#[must_use]
pub struct PyErr {
    exception: Py<PyBaseException>,
}

pub type PyResult<T> = Result<T, PyErr>;
```

`PyErr` owns its exception, with `Py<T>`'s ownership and interpreter
association, rather than marking one pending on the thread state. Dropping
it queues the reference, so recovering in Rust leaves no hidden pending
exception. It is [`#[must_use]`](https://doc.rust-lang.org/std/attribute.must_use.html) to ensure that API users do not unintentionally miss an exception return.

Exception types provide `raise(&mut ctx, message)`, which returns an owned
`PyErr`; the exception is set only when the generated boundary restores it:

```rust
fn checked_size(ctx: &mut ThreadContext<'_>, size: usize) -> PyResult<i32> {
    i32::try_from(size)
        .map_err(|_| PyOverflowError::raise(ctx, "size is too large"))
}
```

Immediately before an error-sensitive C call, its wrapper checks for a
preexisting exception. If present, it takes ownership and returns an error
without making the call. Audited leaf operations may preserve the indicator;
reference release and deallocation follow their save/clear/restore policy.
When a C call fails, its wrapper immediately takes the raised exception
(with `_PyErr_GetRaisedException`, a pointer transfer) while it still
controls the originating state:

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
becomes `SystemError`. Success with a pending exception discards the result
with exclusive context access and the context suspended, and returns the
captured exception.
Failures while creating or reporting an error must leave an appropriate
exception such as `MemoryError`.

At a generated callback boundary, a pending exception turns `Ok` into `Err`,
even if no wrapped operation followed author code. An explicit `Err` remains
primary; secondary exceptions are reported as unraisable. `Err` has its key
checked first; an error from another interpreter is queued under its own key
and replaced with `SystemError`. Restoring an exception may decref the previous
one, so it requires exclusive context access and suspension.

Context availability proves attachment, not ownership of the error
indicator: safe foreign code can set it without crossing a gate. The
initial checks and capture rules keep stale errors out of later C calls.
Function entries return unexpected pending exceptions before invoking author
code; `tp_dealloc` and module free save and restore them (see
[Dropping](#dropping)).

Suspended author closures, including bytes builders, use the same capture
policy. Discarded results and panic payloads are dropped while suspended,
and the indicator is checked again afterwards. During unwinding, restoration
captures secondary exceptions for later unraisable reporting at the boundary;
it neither replaces the original panic nor unwinds itself.

The built-in exception marker trait is sealed because its type pointer goes
straight to C. Custom exception types stored as `Py<PyType>` need checked
binding. Converting Rust error values into a `PyErr` requires an explicit
mutable context; detach/interop exception variants already own a `PyErr`.

### Concrete objects and conversions

Concrete types such as `PyBytes` and `PyModule` are transparent markers with
type-specific methods on `Bound<'py, Marker>`. The set grows with module
needs. `Bound<PyModule>` can add values and types, and
`PyType::new_exception` creates a heap exception type.

`PyType::new_exception` shows why a C function's nominal result type is not
proof: `PyErr_NewException` calls `type(name, bases, dict)`, whose metaclass
can return any object. The wrapper checks the result is a type. The helper
that types a C result is `unsafe`, and Python-choosable results use a checked
downcast.

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
runs with the context suspended. Zero-length requests must not lend the
shared empty-bytes singleton.

#### Conversions

```rust
pub trait FromPyObject<'a, 'py>: Sized {
    fn extract(
        ctx: &mut ThreadContext<'py>,
        obj: Borrowed<'a, 'py, PyAny>,
    ) -> PyResult<Self>;
}

pub trait IntoPyObject<'py> {
    fn into_pyobject(
        self,
        ctx: &mut ThreadContext<'py>,
    ) -> PyResult<Bound<'py, PyAny>>;
}
```

Both take exclusive access and are fallible. Implementations receive an
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

It does not incref or copy:

```rust
let input = data.as_bytes(ctx);
let output = ctx.detach_if_possible(|| compress_without_python(input))?;
```

This proof does not extend to arbitrary buffer exporters (see the
appendix).

Generic implementations compose:

```rust
let value: Option<u32> = FromPyObject::extract(ctx, argument)?;
let object: Bound<'py, PyAny> = value.into_pyobject(ctx)?;
```

Callbacks may return `T` or `PyResult<T>`; only `PyResult` lets the body
raise, though converting `T` can still fail.

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

Only generated `ModuleDef<S>` glue constructs a `ModuleRef<S>`, ensuring a valid
state type and interpreter. Static definitions hold no interpreter-owned
objects; each module instance has its own state, types, and exceptions.

The module token is the static `ModuleDef<S>`'s address, so definitions must
be distinct statics. A private `UnsafeCell` field makes const promotion a
compile error and prevents merging identical statics. `ModuleDef<S>`,
`ModuleRef<S>`, and `ModuleMethodDef<S>` are also invariant in `S` (via
`fn(S) -> S`), per the [safety rules](#safety-rules).

#### Module state

Every module has typed state, even if empty:

```rust
pub trait ModuleState: Send + Sync + 'static + Sized {
    type Gc: ModuleStateGc<Self>;

    fn new<'a, 'py>(
        ctx: &mut ThreadContext<'py>,
        module: ModuleRef<'a, 'py, Self>,
    ) -> PyResult<Self>;
}

// SAFETY: implementations must validate interpreter association, report every
// strong Python edge, and synchronize traversal, mutation, and partial clear.
pub unsafe trait ModuleStateGc<S: ModuleState>: 'static {
    const ENABLED: bool;

    fn validate(value: &S, ctx: &mut ThreadContext<'_>) -> PyResult<()>;
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
    ) -> PyResult<Self> {
        Ok(Self { error: GcSlot::empty(ctx) })
    }
}
```

Traced fields use `GcSlot<T>`, which owns an optional reference with a key
fixed at construction. GC clear permanently closes a slot, so reentrant code
cannot repopulate it:

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
the key before touching the pointer. `get` increfs under protection and
returns `None` if cleared. `replace` returns the old reference so it is
released outside the lock; on mismatch or a closed slot, it returns the
incoming value unchanged inside the error. Replacing with `None` empties
without closing. Nothing exposes `&mut` to the stored handle.

#### Deriving GC policies

`#[derive(ModuleStateGc)]` and `#[derive(PyClassGc)]` classify fields by
their actual type through a sealed trait, since an alias can hide the
spelling:

- `GcSlot<T>` is traced.
- `Option`, `Box`, `Vec`, arrays, and boxed slices of slots are traced slot by
  slot; none can change through `&`. Spelling one directly is a macro error
  asking for direct fields.
- `Arc<GcSlot<T>>` and `Rc<GcSlot<T>>` are compile errors, even behind an
  alias, because shared ownership would double-report edges. The error fires
  after monomorphization.
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

Object traversal must not block, run Python, or hold an exclusive borrow during a
visit; lookups use the `_DuringGC` accessors. Clear releases references
only after the slot is closed and unlocked. External callers of `tp_traverse`
must provide the same exclusion.

#### State lifecycle

CPython creates the module and binds its functions before allocating state;
`PyModule_Exec` then allocates zeroed `StateStorage<S>` (key, atomic lifecycle
state, `MaybeUninit<S>`). An unexecuted module is reachable, from
`importlib.util.module_from_spec` or `sys.modules` mid-import, so every
callback handles missing storage. The macro rejects over-aligned types.

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
) -> PyResult<()>;
```

Publishing stores use release ordering and reads use acquire. Guards apply
failure states on unwind, and a panicking destructor is never retried.
Callbacks see state only once `Ready`, since exec may still be populating
it.

#### Support declarations

Generated modules declare `Py_MOD_GIL_NOT_USED`,
`Py_MOD_PER_INTERPRETER_GIL_SUPPORTED`, the internal exact-version ABI, and
the build mode. That requires auditing extension dependencies and global state.
Process-global Rust caches may hold only Rust data or checked opaque
handles. The identity and lifetime checks apply even when import checks are
disabled.

### Functions

Python-callable, module-scoped functions are ordinary Rust functions whose first two
parameters are the thread context and module state:

```rust
#[pyfunction(signature = (data, /))]
fn standard_b64encode<'a, 'py>(
    ctx: &mut ThreadContext<'py>,
    _state: &Base64State,
    data: Borrowed<'a, 'py, PyBytes>,
) -> PyResult<Bound<'py, PyBytes>> {
    // ...
}
```

A body using only leaf operations may take `&ThreadContext`. `#[pyfunction]`
emits a `METH_FASTCALL | METH_KEYWORDS` trampoline that enters panic
containment and `with_current`, validates the module against its
`ModuleDef<S>` and checks it has executed, binds arguments, calls the
function, and converts the result before `'py` ends. Methods use
`METH_METHOD`.

Argument binding, defaults, and the call live in a generated safe function,
so author code never compiles under `unsafe`. The macros reject `unsafe`,
`async`, `const`, `extern`, and generic functions.

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

`panic_to_owned_error` drops the payload with the context suspended, under a
second catch (a second panic aborts), then builds a fixed-message
`SystemError`. Entry, restoration, and lifecycle guards never unwind; the
outer catch is a last-resort abort.

| Callback kind | Panic policy |
| --- | --- |
| Function, method, getter, setter, and constructor | Set a fixed `SystemError` and return the ABI's `NULL` or `-1` error sentinel. |
| Module create | Set a fixed `SystemError` and return `NULL`. |
| Module exec | Commit the appropriate failure state, set a fixed `SystemError`, and return `-1`. |
| Module or instance clear | Finish the lifecycle transition and set `SystemError`, which CPython reports as unraisable. Instance clear returns failure. Module clear returns 0, because CPython skips clearing the module dict when `m_clear` fails. |
| Traverse | Preserve incoming exception state and propagate the visitor's exact nonzero return and any error it sets. A Rust panic is fatal because continuing with an incomplete reachability report can violate GC invariants. |
| `tp_dealloc` and module free | Save and clear any pending exception before cleanup. Run mandatory non-panicking cleanup guards, then report the panic as unraisable when an attached state is available. Restore the saved exception, or none, afterwards. Abort if cleanup itself cannot be completed safely. |

Before dropping a payload or module state, deallocation commits the
destroyed state and installs cleanup guards, so a panic cannot cause a
double drop or skip cleanup. Destructors run with the context suspended and
receive no context. During finalization, suspension keeps cleanup
restrictions in force.

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

    fn validate(value: &C, ctx: &mut ThreadContext<'_>) -> PyResult<()>;
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

The class token is the address `definition()` returns, and downcasts trust
it. If two classes returned identical promoted constants, the compiler could
merge them and one class's payload would be read with the other's layout, so
`ClassDef` has the same private `UnsafeCell` field as `ModuleDef`. The class
marker trait is sealed. Generating the static through an `unsafe` or sealed
trait, as `export_module!` does, would remove the remaining reliance on a
safe method.

`ClassDef<C>`, `PyType<C>`, `PyInstance<C>`, the generated method tables, and
`GcSlot<T>` are invariant in their parameter. Otherwise safe code could
coerce a `Payload<fn(&str)>` type object to `Payload<fn(&'static str)>`, call
`new_instance`, and downcast back successfully. Handles may be covariant in
their marker, because every parameterized marker is invariant.

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
payload slot's key before publishing. It allocates with
`_PyType_AllocNoTrack` (`PyType_GenericAlloc` tracks too early), writes the
payload and lifecycle metadata, then calls `PyObject_GC_Track`. On failure or
unwind, the unpublished payload is dropped with the context suspended.

Methods recover the defining module through `METH_METHOD`, validate its
state and key, and bound borrows by owner and entry. They receive `&self`,
never `&mut self`, since methods can be called concurrently on free-threaded
builds; mutable state needs interior mutability.

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
| Allocate | Validate payload associations against the type/context key; write `C` into an untracked object, publish ready, then call `PyObject_GC_Track`. |
| Traverse | Visit the actual heap type and payload edges through shared access; only call `visitproc`, propagate its exact return, and never invoke Python or block independently of the visitor. |
| Clear | Remove payload edges through synchronized `GcSlot` access using `&C`; release removed references after leaving slot protection; later clears are no-ops. The instance retains its type until deallocation. |
| Deallocate | Untrack first. Contain any required clear independently; then commit dropping, drop `C` once, run allocation/type guards, and report the first panic. |

Types are immutable, final (preserving the `METH_METHOD` defining-class
link), and non-instantiable unless they define `tp_new`.

#### Methods

`#[pymethods]` accepts:

- `#[pyfunction]`: an instance method with a `PyCMethod` trampoline, taking
  `&C::State`;
- `#[getter]` and `#[setter]`: halves of a `ClassGetSetDef<C>`;
- `#[new]`: a `tp_new` trampoline that parses arguments, constructs `C`, and
  moves it into a new allocation.

It emits `C::PY_METHODS`, `C::PY_GETSETS`, and `C::PY_NEW`, installed with
`methods`, `getsets`, and `constructor`. All function-glue rules apply.

### Argument parsing

```rust
#[pyfunction(signature = (data, value = 1, /))]
fn checksum<'a, 'py>(
    ctx: &mut ThreadContext<'py>,
    state: &ChecksumState,
    data: Borrowed<'a, 'py, PyBytes>,
    value: u32,
) -> PyResult<u32> {
    // ...
}
```

The macro checks names and emits a static `ParamSpec`. Values are extracted
with `FromPyObject`; defaults are Rust expressions. Vectorcall binding uses a
fixed stack array and allocation-free keyword matching, yielding
`Borrowed<'args, 'py, PyAny>` slots with no refcount churn. Constructor
tuple/dict parsing fills the same slots.

The glue checks its own inputs and keeps every argument alive while it is
in use:

- A `ParamSpec`/slot-count mismatch is a checked error.
- `kwnames` must be a tuple before its entries are read.
- Constructor keyword values are never borrowed from the `kwargs` dict.
  Converting one argument can run Python code, such as `__index__`, that
  removes another value from the dict, which would free a borrowed value.
  Before converting any argument, the parser takes its own references to
  every keyword value, and it keeps them until the call and result
  conversion finish. It stores them in a private tuple rather than a list,
  because Python code can find a list through `gc.get_objects()` and clear
  it.
- Positional arguments and vectorcall arrays can be borrowed without a
  copy: the caller keeps them alive for the whole call, and Python code
  cannot modify them.

Argument errors raise `TypeError`.

## Testing

CI runs `cargo fmt` and [Clippy](https://github.com/rust-lang/rust-clippy),
and checks that every Python-touching safe `c_api` wrapper takes a context
or sealed capability, and that shared-context wrappers are audited as leaf
operations.

A new `_testrustapi` module, run by the regular test suite, covers testing the new Rust API correctness.

### Performance

Rust code must match or be very close to C code in performance. Rust APIs should be measured against equivalent C: leaf operations, binding, entry (outermost and
nested), suspension, slot access, explicit decref, inline drops, queue
overflow, contention, tiny callbacks, compression throughput, retained
memory, gate overhead on code that never enters Rust, detached versus
attached-fallback CPU work, and output copying versus `PyBytesWriter`.

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
    memory on reattach. Leases make this reliable; the fatal hang check does
    not fix it.
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
- **Shutdown progress.** Leases let a daemon thread blocked in a callback
  under a Rust entry delay shutdown indefinitely. This should be re-evaluated.
- **Mutable GC containers.** Some constructs need a collection of traced
  Python references that changes after the object is created: a cache
  such as `functools.lru_cache`, a registry of callbacks, or the set of
  pending tasks in `_asyncio`. The API cannot express these yet. Traced
  edges live only in `GcSlot<T>` or in containers of slots that cannot
  change through `&`, and payloads are only reachable through `&self`. Such
  a module today has to keep its references in untraced `Mutex`, where cycles through them leak. Mutable containers
  such as a `GcVec<T>` or `GcMap<K, T>` would fill that gap, but each must
  prove what `GcSlot` does:
  - every reference is checked against the container's interpreter key
    when it is inserted, not at GC time;
  - traversal reports each strong reference exactly once;
  - traversal reads without locking, so no thread may be paused
    mid-mutation while holding the container's lock;
  - clear closes the container, so finalizers cannot re-add edges, and
    releases references only after the lock is dropped.

  A naive `Mutex<Vec<Py<T>>>` fails the third requirement. Traversal
  cannot take the mutex, because its holder may be paused by
  stop-the-world, but reading without the lock races with a `push` that
  reallocates and frees the buffer.
- **Borrowed `GcSlot` reads.** `GcSlot::get` increfs on every read, because
  another thread can replace the slot and decref the old value. A guarded
  read could instead return a `Borrowed` tied to a guard that blocks
  replacement. Since a slot's lock holder must never park or run callbacks,
  the guard would have to allow only leaf operations, for example by
  borrowing `&ThreadContext` for its whole lifetime. Whether a reader may be
  paused on a contended lock while holding the guard is still open.
- **Explicit thread state C APIs** and inlining the entry counters could speed hot
  paths without changing the Rust API.
- **Panic strategy.** The API requires `std` and `panic=unwind`. A
  `panic=abort` build is ABI-safe but cannot recover; a `no_std` build would
  need panic-abort or another no-unwind boundary.
