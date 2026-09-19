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

1. be sound on GIL and free-threaded builds, including subinterpreters with
   separate GILs;
2. support every PEP 11 platform;
3. approach equivalent C performance where safety permits;
4. feel familiar to PyO3 and C API users; and
5. use modern APIs such as `PyModExport`.

Every use of `unsafe` requires a `// SAFETY:` comment stating its proof
obligation. No panic may escape a non-unwinding C ABI.

### Non-goals

Full C API parity, a stable ABI, and sharing of Python objects
between interpreters are out of scope. Wrappers are added as modules implemented in Rust need
them.

### Specific guidelines

- Encode CPython contracts in the type system where practical.
- Use macros sparingly to control compile time.
- Preserve Rust's initialization, aliasing, and concurrency invariants.
- Support both GIL and free-threaded builds.

### API design

1. Bindings and the FFI boundary
2. Interpreter identity and lifetime
3. Managing the thread context
4. Python object handles and garbage collection
5. Deferred reference release
6. Exceptions and error handling
7. Concrete object abstractions
8. Rust/Python conversions
9. Module definition
10. Function definition
11. Class and method definition
12. Argument parsing
13. Testing
14. Open decisions and future work

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
touch Python requires a borrow of `ThreadContext`, or the private restricted
capability appropriate to traversal or reference release. Shared context
access is reserved for audited leaf operations; operations that can run
callbacks or change attachment require exclusive access. The scope protocol
below maintains attachment without a TLS check at every leaf operation.
An operation without a capability needs an audited proof that its entire
call path is thread-state-free. Ordinary `Drop` only submits releases; it
never obtains a capability to decref synchronously. Existing C APIs may
still obtain the current state internally.

#### Interpreter identity and lifetime

Each interpreter lifetime has a private `InterpreterKey`, represented by a
nonzero 64-bit generation number. CPython allocates it from a process-wide
sequence that survives `Py_FinalizeEx` and reinitialization; exhaustion fails
interpreter creation rather than reusing a key. A main interpreter created
after reinitialization receives a new key too. Existing interpreter IDs alone
do not satisfy this contract: `Python/pystate.c` currently resets `next_id`
when enabling the interpreter registry.

The key records the interpreter under whose API contract a reference was
acquired and is usable, not which interpreter allocated its storage. CPython
may supply process-global constants or static types to several interpreters.
An accessor called in interpreter B may return a B-associated handle to such
an object even if its storage originated in the main interpreter. An existing
A-associated handle cannot be relabeled B merely because the pointer is
immortal or is equal to another pointer. Only an audited CPython contract can
authorize sharing; ordinary cross-interpreter transfer reconstructs a new
object through a separately audited conversion.

There is no generic object-to-interpreter lookup. `PyObject` headers do not
carry this association, and the free-threaded `ob_tid` describes refcount
thread ownership, not interpreter identity. Generated glue obtains the key
from the attached thread state's interpreter and preserves it through handle
conversions. Reading the current interpreter and its cached key requires no
allocation, registry search, or lock. No `PyObject` header change is required.

Identity does not prove liveness. Each active outermost Rust entry holds an
interpreter-finalization lease, including while temporarily detached. Nested
entries for the same protected thread state reuse that protection. The lease
is held by private entry glue, cannot escape to user code, and is released on
normal return and unwind after all scoped values have been consumed, queued
releases have been drained or transferred, and panic reporting has finished. Safe
APIs must not initiate or synchronously wait for destruction of that
interpreter while holding its lease. A stored `Py<T>` does not hold a lease:
otherwise module state could prevent the shutdown needed to destroy itself.

The lease follows the lifetime protection of
[PEP 788 interpreter guards](https://peps.python.org/pep-0788/), using a new
private stack-based entry API to avoid allocating a public guard on each
callback. Acquisition must synchronize with finalization's admission barrier;
checking a finalizing flag before incrementing a counter is insufficient.
Normal entry fails before exposing Rust references if admission has closed.
Generated callbacks that CPython legitimately invokes during finalization
instead receive private cleanup authorization from the finalizing thread.
This also covers synchronous reentrant callbacks caused by those finalizers.
Their contexts are limited to that callback's lifetime and cannot request
explicit detachment or unrestricted foreign-library interoperation. Audited
synchronous C calls may temporarily detach internally only if the cleanup
authorization protects their state through restoration and finalization
cannot advance until the call returns.
Neither user code nor an arbitrary foreign thread can manufacture cleanup
authorization. Traversal has its separate restricted visitor capability and
cannot acquire a general context merely to bypass its restrictions.

The initial API creates contexts only at generated C entries. A future native
thread entry API must select an explicit interpreter key, acquire lifetime
protection before attachment, and report shutdown failure without a Python
exception when detached. It must not use `PyGILState_Ensure` to implicitly
select the main interpreter. Raw C callers remain responsible for presenting
valid callback inputs and obeying CPython's interpreter contracts.

#### Managing the thread context

`ThreadContext<'py>` is a scoped, exclusive capability for one exact CPython
thread state on the current OS thread. Whenever safe code can use the
context, that state is attached and protected against finalization. It is
not enough for some state in the same interpreter to be attached: exception
state and other thread-local CPython state belong to the exact thread state.

```rust
pub struct ThreadContext<'py> {
    ptr: NonNull<ffi::PyThreadState>,
    interpreter: InterpreterKey,
    scope: NonNull<ScopeRecord>, // private, stable storage in the entry guard
    #[allow(clippy::type_complexity)]
    _marker: PhantomData<(fn(&'py ()) -> &'py (), *mut ())>,
}
```

The function marker makes `'py` invariant, and the raw-pointer marker makes
the context `!Send + !Sync`. It is neither `Copy` nor `Clone`; only generated
entry glue constructs it and lends it to author code. `Bound` and `Borrowed`
are also invariant in `'py`. This is a fresh entry-scope brand, not a lifetime
chosen once per interpreter: separate entries cannot mix scoped values
directly, even in the same interpreter. Unbind and checked bind explicitly
move ownership between scopes.

There are two classes of Python access:

| Context argument | Permitted operations | Examples |
| --- | --- | --- |
| `&ThreadContext<'py>` | Audited leaf operations that preserve attachment and cannot invoke arbitrary Rust or Python callbacks, run deallocators, or reach a suspending/reentrant safepoint. | Checked binding, ownership-preserving casts, exact immutable bytes access, and an audited incref. |
| `&mut ThreadContext<'py>` | Operations that may execute Python, call user code, allocate through a potentially reentrant path, decref, drain releases, detach, or switch states temporarily. | Generic extraction, object creation, attribute access, explicit destruction, and `detach`. |

The classification covers the entire call path, including failure handling,
allocator hooks, synchronization, and incidental cleanup. A shared-context
operation returns native errors if reporting a Python error would require
exclusive access. A new wrapper defaults to exclusive access until its leaf
contract has been audited. A blocking acquisition is not a leaf merely
because its successful fast path is short. Attachment does not itself
protect object contents against concurrent mutation on a free-threaded build.

Author functions may accept `&mut ThreadContext` and reborrow it as shared
for consecutive leaf operations. Taking `&mut ThreadContext` does not make
it unavailable automatically. A private wrapper suspends the capability only
around the actual potentially reentrant C call or closure; it never passes
that suspended context to author code, trait implementations, or callbacks.
Argument conversion and return conversion implementations receive an
available context and use the same wrappers as other Rust code.

##### Entry and attachment transitions

This design requires new private CPython support. Rust borrowing alone
cannot stop a different safe Rust library from calling `PyEval_SaveThread`
without borrowing this context. Checking only when a generated callback is
entered is also insufficient: a detached Rust closure can call scoped TLS
without crossing a generated C entry.

CPython maintains a stack of Rust scope records per OS thread, shared across
all extension libraries and all interpreters on that thread. A record is
**available** while its context can be used, or **suspended** while a private
wrapper holds the exclusive borrow and exposes no access to that context.
At most one record is available on an OS thread. Nested entries are admitted
only while all enclosing records are suspended; they create a fresh scope
for the state actually attached at that entry. Scope depth on an individual
`PyThreadState` is insufficient when C code switches interpreters.

The required private operations have these contracts; their names and C
layout are implementation details:

1. **Enter:** verify the actual attached state and execution phase, acquire
   or reuse interpreter lifetime protection, and register an available record
   in stable, private stack storage. Entry cannot publish a context before
   all these steps succeed. A per-interpreter lease does not by itself keep
   an individual thread state alive: a registered entry also protects its
   exact state from deletion until restoration and exit.
2. **Suspend:** while holding `&mut ThreadContext`, mark its record suspended
   before invoking a potentially reentrant operation. Native preflight checks
   leave attachment and registration unchanged on refusal and do not create
   a Python exception. No arbitrary callback occurs between preflight and
   committing suspension. The private restoration guard retains the exact
   original state, record, and lifetime protection.
   Suspension removes access to the Rust capability; it does not broaden
   the C operations permitted by the execution phase. Cleanup authorization
   and its transition restrictions remain in force.
3. **Transition:** CPython's attach, detach, swap, and thread-state deletion
   paths reject any transition that invalidates an available record. They
   consult the OS-thread registration even when there is currently no
   attached state, and cover both public and internal transition paths.
   Deletion additionally respects every live thread-state protection,
   including suspended records and active consumers, even when attempted
   from another OS thread. The deleting thread's own scope stack is not
   sufficient. Destruction cannot synchronously wait for a protected entry
   on its own call stack to finish.
4. **Resume:** require nested records to have exited and the exact original
   state to be attached before making the outer record available again.
   Restore it on normal return and Rust unwind according to the operation's
   audited contract. If a foreign call violates its restoration contract,
   fail without exposing a stale context; do not silently attach over an
   unknown state. Failure to restore safely is fatal.
5. **Exit:** after author code and scoped handles are finished, mark the
   record unavailable and service or transfer its releases while retaining
   the sink, ancestry, and lifetime protection for reentrant cleanup. Remove
   the record only after its sink is empty. Cleanup and panic reporting
   precede release of lifetime and thread-state protection.

Registration and restoration guards are private and cannot be returned or
forgotten by safe author code. Forgetting a public handle may leak a reference
but cannot remove attachment protection or strand a suspended registration.
Stable record addresses are established before registration, without requiring
an allocation for each entry. The entry guard, rather than the public context,
owns the record and its associated release sink.

A controlled Rust wrapper checks detachment permission before calling
`PyEval_SaveThread`, so that C API needs no new recoverable failure result
for this path. Legacy C transitions that bypass the protocol while an
available record exists must fail-stop before changing attachment; silently
continuing would invalidate safe Rust's proof, and waiting on the same thread
would deadlock. A new private checked transition API may return a native
status instead. Scope-entry protocol violations likewise fail before exposing
Rust values. These requirements must be implemented in CPython before leaf
operations can omit attachment checks; this is not a safe model for an
unmodified runtime.

Audited internal parking for stop-the-world or synchronization can be
exempted only if no arbitrary callback or Rust access can run until the exact
state is restored, and the leaf operation exposes no attachment-dependent
access during that interval. Otherwise it must use suspension. This audit
includes free-threaded safepoints and object critical sections, not only
`Py_BEGIN_ALLOW_THREADS`.

Entry uses a higher-ranked closure so callers cannot choose `'py = 'static`
or return a scoped value. Its conceptual private signature is:

```rust
// SAFETY: only generated C callbacks with an attached state and valid inputs
// may enter; the implementation performs registration and lifetime admission.
unsafe fn with_current<R>(
    f: impl for<'py> FnOnce(&mut ThreadContext<'py>) -> R,
) -> Result<R, EntryError>;
```

`for<'py>` makes the lifetime fresh and `R` cannot depend on it. Generated
wrappers convert scoped results to raw C values before returning. Macros
reject author functions that request a specific lifetime such as `'static`.
Callback inputs also have a separate owner lifetime, described under argument
parsing. The actual entry glue keeps its private guard through parsing,
conversion, unwind handling, release processing, and exception reporting.

`EntryError` is native. Normal admission failure is translated by private
callback-specific reporting glue with its own valid attachment and lifetime
authorization; it must not manufacture a general context after admission
closed. If the callback has neither normal nor cleanup/reporting
authorization, or violates the scope protocol, it cannot safely continue.
No error path may consult a stale context to raise an exception.

##### Detachment, reentry, and foreign wrappers

```rust
impl<'py> ThreadContext<'py> {
    pub fn detach<R>(
        &mut self,
        f: impl FnOnce() -> R,
    ) -> Result<R, DetachError>;

    pub fn interop<R>(
        &mut self,
        f: impl FnOnce() -> R,
    ) -> Result<R, InteropError>;

    pub fn drain_releases(&mut self);
}
```

`detach` checks the execution phase, suspends the record, calls
`PyEval_SaveThread` or its private equivalent, executes `f` on the same OS
thread, and restores the saved state before resuming the record. Its private
guard does the same restoration if `f` panics. The interpreter lease and
thread-state protection span the entire detached interval. A refusal returns
a native `DetachError` before detaching; a caller can subsequently translate
it through an available context. Cleanup entries refuse explicit detachment.
There is no outermost-entry permit: a normal nested callback may detach when
all enclosing contexts are suspended and it has lifetime protection.

`interop` suspends the record without initially detaching. Its closure can
use a foreign safe wrapper that manages its own attachment, including
switching interpreters. That wrapper must restore the incoming state before
returning or unwinding; the boundary verifies exact restoration before
resuming the context. Cleanup entries refuse unrestricted interoperation.
Unsafe foreign calls retain their ordinary C and Rust safety obligations.
This boundary protects this API's contexts; it does not repair a foreign
wrapper's own unsound attachment tokens or reference lifetimes. Foreign
wrappers remain responsible for their capabilities and C API contracts.

Neither closure receives the borrowed context. Neither needs `Send` bounds
on its captures or result: both execute on the same thread, and the proof is
exclusive access to the capability, not an assertion about captured values.
Opaque scoped handles and independently safe Rust data may be moved into or
returned from the closure. Accessing Python through a handle still requires
an available matching context. Binding it in a fresh callback requires an
explicit unbind and checked bind.

This addresses the mechanisms in PyO3 issues
[#2141](https://github.com/PyO3/pyo3/issues/2141) and
[#3640](https://github.com/PyO3/pyo3/issues/3640). `SendWrapper` can make a
non-`Send` value transportable while allowing access on its originating
thread, and a capture-free function can reach scoped TLS. Neither property
proves attachment. In this API, placing a shared context reference in scoped
TLS prevents an overlapping exclusive borrow for detachment or reentrant
operations. A sound mutable scoped-TLS abstraction or `RefCell` must retain
its exclusive loan throughout the call; attempted reentrant borrowing fails
rather than yielding an alias. TLS may make opaque handles reachable while
detached, but cannot supply a usable suspended context. Foreign detachment
that does not borrow the context is covered by CPython's transition gate.

The same rule prevents an outer context for interpreter A from being used by
a TLS hook invoked while a C call has switched to interpreter B. The call
holds the exclusive borrow of A's context for its entire duration; B's
callback receives a distinct scope. Merely promising to restore A when the
C call eventually returns would not be sufficient.

A Python-touching implicit operation cannot bypass these rules. Handle
`Debug`, equality, hashing, indexing, or iterator operations either use only
Rust metadata or require an explicit context through a named method; no
implicit operation performs Python `repr`, equality, or protocol dispatch.
Ordinary owning-handle `Drop` only queues releases, as specified below.

#### Python object handles and garbage collection

Three private generic handle representations distinguish borrowing, ownership,
and attachment:

| Handle | Owns a reference | Lifetime | Auto traits | Drop |
| --- | --- | --- | --- | --- |
| `Borrowed<'a, 'py, T>` | No | owner `'a` and attachment `'py` | `Copy`, `!Send`, `!Sync` | none |
| `Bound<'py, T>` | Yes | guarded entry `'py` | `!Send`, `!Sync` | enqueue in entry release sink |
| `Py<T>` | Yes | unbound, for stored state | `Send`, `Sync` | enqueue by interpreter key |

The two `Borrowed` lifetimes are independent: the reference owner supplies
`'a`, while `ThreadContext` supplies interpreter liveness and permission to
access the object within `'py`. `Py<T>` may cross threads, but touching its
object still requires an active context for its associated interpreter.

`Py<T>` stores a non-null object pointer and an `InterpreterKey`. `Bound`
stores an object pointer and a private pointer to its entry's release sink,
plus the invariant scope brand and thread-affinity marker. The sink records
the interpreter key and is valid for all of `'py`, including detached and
suspended intervals. Unbinding copies that key into the resulting `Py<T>`.
A borrowed handle needs only the pointer plus lifetime markers because its
checked construction binds it to the context's scope. On a 64-bit target,
both owning forms can remain two words; padding is target-dependent.

The scope brand proves association and lifetime, not independent permission
to touch Python. A `Bound` or `Borrowed` can exist while its context is
suspended, but Python access still requires an available context. They cannot
create a context or expose a safe raw-object dereference on their own.

Every safe `Py<T>` operation that accesses the object, including binding,
downcasting, cloning a reference, conversion, and explicit destruction, must
first compare the handle's key with the context's key. Mismatch is a native
`InterpreterMismatch` error and must be detected before dereferencing the
object, even to inspect its type or refcount. This also rejects an expired
handle when a later interpreter occupies the same address. The active
context protects the matching interpreter for the whole operation.

The binding signature is:

```rust
impl<T> Py<T> {
    pub fn bind<'a, 'py>(
        &'a self,
        ctx: &ThreadContext<'py>,
    ) -> Result<Borrowed<'a, 'py, T>, InterpreterMismatch>;
}
```

Scoped operations accept the same invariant `'py` on all handles and the
context, avoiding repeated dynamic identity checks. Leaf access also needs
no repeated attachment check: entry registration and transition enforcement
maintain that invariant until the next exclusive operation. To propagate a
native mismatch through a Python callback, explicitly raise an exception on the
current context; no error conversion implicitly consults ambient attachment.

Moving among the forms is explicit:

```rust
let stored: Py<PyType> = exception_type.unbind();
let current: Borrowed<'_, 'py, PyType> = stored.bind(ctx)
    .map_err(|err| err.raise(ctx))?;
let retained: Bound<'py, PyType> = current.to_owned(ctx);
```

Neither owning handle implements `Clone`; `clone_ref(ctx)` and
`to_owned(ctx)` make each incref explicit. Raw borrowed and owned-pointer
constructors are private. Their glue proves non-nullness, interpreter identity,
liveness, and the C ownership contract before constructing a handle. Merely
having an attached thread state does not validate an arbitrary raw pointer.
Upcasts, unbinding, and ownership transfers preserve the key; generated
callback arguments and audited API results acquire the current scope's key.

All Rust references into Python-owned memory, including byte slices, class
payloads, and module state, are bounded by both the owner's lifetime and the
guarded entry scope. A `Py<T>` in static Rust storage must not yield a static
`&[u8]`, `&C`, or `&S`. A view requiring continuous attachment additionally
borrows the context, excluding exclusive operations for its entire lifetime.
A Rust-only `Send + Sync` payload or immutable bytes view may span detachment
only under an audited contract retaining its owner and entry protection.
This does not authorize Python-field access without a matching context.
Safe code cannot return Python-backed references from the higher-ranked entry.

An attachment borrow is not a lock or a proof of exclusive object access.
Another thread can mutate a Python object, and a foreign wrapper can reenter
Python on the same state without detaching. Therefore neither the transition
gate nor `&ThreadContext` justifies a Rust slice into mutable container storage,
an unsynchronized `&Cell<_>` payload, or an exclusive reference to published
Python memory. Such views need separate ownership and synchronization proofs.
`Send + Sync` remains mandatory for shared class and module payloads.

Owning handles implement consuming explicit destruction:

```rust
object.drop_with(ctx)?;
```

`Bound::drop_with(&mut ctx)` uses the shared scope brand and returns `()`.
`Py::drop_with(&mut ctx)` returns `Result<(), InterpreterMismatch>`; on
mismatch it consumes the handle through ordinary queued `Drop` and reports
the native mismatch without touching the foreign object. A Python callback
maps that error with `err.raise(ctx)`. On a match, explicit destruction uses
the exclusive-call protocol to release the reference directly, and consumes
the ownership exactly once. As elsewhere, `ctx` that is already a mutable
reference is passed directly, without adding another `&mut`.

Ordinary `Drop` never decrefs, drains releases, attaches, or switches states,
even when the current thread happens to be attached to the right interpreter.
A direct decref could invoke a finalizer which reenters Python or detaches
while a shared context remains reachable through scoped TLS. Actual-attachment
checks in `Drop` would not establish the exclusive borrow needed for that
execution. `Bound::drop` instead appends to its entry's release sink; `Py::drop`
submits by key, with an optional matching live sink fast path. Both transfer
ownership without reading the object's header or invoking Python.

Explicit release and queue draining preserve the incoming Python exception.
Generated Rust deallocators contain panics and preserve exception state while
reporting an unraisable error. `Drop` must not unwind, including when Rust is
already unwinding. Detached destruction is supported behavior; allocation
failure or a closed/expired interpreter uses the abandonment policy below,
rather than a decref in the wrong execution context.

`PyAny` is the erased object marker. More specific markers, such as `PyBytes`
and `PyType`, have the same transparent layout, so upcasts to `PyAny` do not
touch the refcount. There is intentionally no `&mut PyAny`: Python objects
have interior mutability and can be changed through aliases, especially in a
free-threaded build.

Cyclic GC tracking is handled for modules and classes and described in their
respective sections. A tagged `Py<T>` stored in pure Rust state may remain
opaque even if its interpreter expires. Publishing a handle as a traced
Python edge additionally requires association with the containing object's
interpreter, both at construction and every later mutation. A binding check
at eventual use is too late to protect the collector.

#### Deferred reference release

Each Rust entry has a private release sink with a small inline buffer.
`Bound::drop` appends through audited interior mutability in that sink, with
no allocation, TLS lookup, global lock, or Python call while capacity remains.
The entry owns the buffer; no queue node points into the dropped handle.
When the buffer fills, it transfers a batch to the interpreter's native
queue, or obtains Python-free overflow storage. It must never make room by
decrefing in `Drop`. If both options fail, retain every previously accepted
reference in the sink and abandon only the incoming, unaccepted reference
under the policy below. Acceptance includes insertion into an inline sink;
later allocation failure cannot revoke it. No mutable borrow of sink storage
or queue lock spans a callback.

The sink remains valid through suspended contexts and Rust unwinding.
`Py::drop` may use a CPython-owned OS-thread lookup of a live sink with the
same generation key, including a suspended outer entry's sink, without
inspecting Python memory. Otherwise it submits to the interpreter queue.
This optimization carries the same admission and ownership guarantees as
ordinary submission; a stale sink cannot survive entry exit. Entry exit
drains or transfers every buffered reference before retiring the sink. If
transfer fails, it drains the retained entries under private release
authorization and lifetime protection; it cannot abandon an accepted batch.
After external admission closes, authorized cleanup sinks use their private
cleanup route for overflow too, or retain accepted entries for local draining.
That route uses accepting cleanup storage or Python-free overflow allocation;
it must not recursively resubmit to the same full inline sink.

CPython owns one native release queue per interpreter lifetime. The following
new private API is a required part of this design, not an existing C API:

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

The caller supplies exactly one owned reference associated with the key, or
the opaque pointer remaining from such ownership after the interpreter has
expired. Only `_Py_RELEASE_QUEUED` consumes that reference. The queue takes
over the existing reference without an incref; the producer must not also
decref it. Failure consumes nothing, sets no Python exception, and does not
inspect the object. Keys and pointers are private to the safe Rust wrapper;
the raw C API retains these ownership and association preconditions.

Submission is callable from any native thread: detached, attached to another
interpreter, or after `Py_FinalizeEx`, while the CPython library remains
loaded. It must not read the object's header, change its refcount, invoke
Python, attach a thread state, or panic. Queue storage and diagnostic paths
must have an audited Python-free allocation/synchronization contract. A
short native lock or allocation may contend, but submission must not wait
for the target interpreter to execute, acquire its GIL, or finish finalizing.
Pooled queue nodes and batched draining amortize submission overhead. A
node owns its copied pointer; it must not refer to the Rust handle's former
stack or container location.

A process-lifetime registry maps generation keys to live queue endpoints.
Registry synchronization survives runtime reinitialization. Lookup, admission,
and any notification that accesses interpreter or thread-state memory are
protected against interpreter deletion as one protocol. No unprotected
`PyInterpreterState *` escapes that protocol. Retirement removes the endpoint
before freeing interpreter memory; absent keys are expired and never alias a
new interpreter. In-flight producers protect only submission bookkeeping,
not an indefinitely retained Python handle. Notification must remain reliable
when the ordinary pending-call queue is full: an accepted release cannot
disappear merely because scheduling a wakeup failed.

Before removing a normal batch, the consumer acquires one finalization lease
for that batch or reuses an enclosing entry's protection. It retains this
protection through decrefs, reentrant callbacks, and batch cleanup, including
any temporary detach inside a C deallocator. Shutdown accounts for these
active consumers as well as producers: an empty queue does not prove that a
previously removed batch has finished. This is one lease per batch, not per
reference. The final shutdown drain instead uses cleanup authorization.

The consumer takes a batch out of the queue and drops its native locks before
decrefing any object. Each queued reference is released exactly once while
attached to its associated interpreter with permission to execute
deallocators. Until then the queued ownership is an external strong reference
for GC purposes; moving an edge out of a GC field must remove that field's
edge before publishing the release. The consumer preserves the incoming
exception and tolerates finalizers that reenter Rust or submit more releases.
It must not hold an incompatible Rust payload borrow across a decref.

Draining requires the exclusive-call protocol, either through
`ctx.drain_releases()` or private release authorization between scopes.
CPython checkpoints defer draining while any context on the current OS
thread is available; being attached to the right interpreter is insufficient.
An explicit drain suspends its context for the whole batch and restores it
after all reentrant callbacks have finished. Eligible entry boundaries retain
the entry lease while servicing releases after the scope ends.

Draining uses bounded batches during normal execution and also runs as part
of interpreter shutdown. It must not depend solely on another call into a
Rust module. Deferred release promises neither destruction before `Drop` returns
nor destruction on the submitting OS thread. Without target execution it can
retain resources until shutdown; native resources that need prompt release
should expose an explicit close operation with a context.

Shutdown uses the following admission protocol:

1. While open, the queue accepts references and can be drained normally.
2. Finalization closes admission of new external Rust-entry leases and lets
   existing entries and admitted normal consumers, including their
   detach/reattach operations and nested callbacks, finish while the
   interpreter is intact. It continues servicing releases under cleanup
   authorization, releasing its GIL and any locks needed by those entries
   while waiting. New normal consumer leases use the same closed gate.
3. Queue admission closes atomically against producers. Already admitted
   submissions finish and all accepted references are drained while the
   interpreter can still execute deallocators. Waiting and
   decrefing occur without registry/queue locks and without keeping the world
   stopped across work that may need another thread. This must integrate
   with `Python/pylifecycle.c`'s pre-finalization loop, not be inserted after
   that loop returns holding its pending-call lock with the world stopped.
4. Authorized finalization callbacks still enqueue their ordinary drops into
   a cleanup sink. After external admission closes, CPython accepts this
   sink's releases only under the finalizing thread's private cleanup
   authorization and matching key. Drain cleanup sinks and their reentrant
   submissions to completion before tearing down object-supporting state;
   this also covers drops produced by later module and type cleanup.
   Explicit release still requires exclusive context access. New external
   submissions receive `CLOSED`; after endpoint retirement they receive
   `EXPIRED`. No accepted queue or sink entry survives interpreter teardown.

An ordinary `atexit` drain or a check-then-enqueue sequence does not establish
this protocol. The final drain's reentrant callbacks remain under cleanup
authorization, and may not acquire a new normal entry lease or request
explicit detachment. Emptying one batch is not sufficient if its callbacks
created more cleanup releases. The any-thread API may route a matching call
from an authorized cleanup scope to that scope's sink; it must never infer
authorization merely from being attached to a finalizing interpreter.

On `CLOSED`, `EXPIRED`, or `NO_MEMORY`, Rust `Drop` deliberately abandons the
unreleasable reference and records a Python-free, non-reentrant,
non-allocating, non-panicking diagnostic. It never retries by dereferencing
the pointer or decrefing in a different interpreter. This policy can leak
resources, but cannot make an expired object accessible through safe Rust.
The same rule applies to native shutdown and TLS destructors. Safe Rust can
also forget a handle, so correctness cannot rely on every destructor running.

This queue guarantees release of accepted references before teardown, not
reclamation of every `Py<T>` retained outside the interpreter. Releasing
those references during shutdown would require registered, revocable
ownership records; guaranteeing enqueue under allocation failure would
require reserved storage. Neither cost is imposed on every owning handle in
this design. The common scoped drop uses the inline sink; explicit destruction
can avoid queue traffic when prompt release matters. Deferred release changes
finalizer timing even on attached threads, and batching may retain memory
longer. Benchmark the inline path, overflow, and unbound `Py` submissions;
there is no claim that queuing every implicit drop is free.

On platforms supporting `fork`, the registry, sinks, scope records, and
lease counts must participate in CPython's before/after-fork protocol.
Detached producers must be quiesced or synchronized by that protocol;
stopping attached Python threads alone is insufficient. The child must
reinitialize native synchronization, preserve accepted releases for surviving
interpreters, remove counts and records from vanished threads, and retire
endpoints for interpreters removed after fork. Records and restoration guards
on the surviving thread remain valid. Fork preparation cannot wait while
holding a lock that a paused producer needs. The queue is not ready for
general use until these child and parent paths are defined and tested.


#### Exceptions and error handling

Errors own their exception rather than marking a pending exception in an
ambient thread state:

```rust
#[must_use]
pub struct PyErr {
    exception: Py<PyBaseException>,
}

pub type PyResult<T> = Result<T, PyErr>;
```

`PyErr` has the ownership and interpreter association of `Py<T>`. It can move
between Rust threads, but can be inspected or restored only through a context
for its associated interpreter. It is not tied to the thread state on which
the exception was created. Dropping or replacing it enqueues its reference;
recovering from an error in Rust therefore does not leave a hidden exception
pending on the thread state. No ambient `PyErrRaised` token or freshness
tracking is needed.

Built-in and custom exception types provide `raise(&mut ctx, message)` which
constructs and returns an owned `PyErr`. The name describes the intended
Python error; the exception is not left pending until the generated boundary
restores it. Exception construction may allocate and reenter, so it follows
the exclusive-call protocol:

```rust
fn checked_size(ctx: &mut ThreadContext<'_>, size: usize) -> PyResult<i32> {
    i32::try_from(size)
        .map_err(|_| PyOverflowError::raise(ctx, "size is too large"))
}
```

A C wrapper applies the function's complete error contract. Ambiguous numeric
sentinels also require inspecting the error indicator. Once failure is known,
it immediately takes ownership of the raised exception and clears the error
indicator before any unrelated Python operation or release processing. The
private capture happens while the wrapper still controls the exact originating
state, before resuming its context:

```rust
// Within the private C-call wrapper, with its exclusive borrow held:
if ptr.is_null() {
    // SAFETY: the audited C API's NULL result guarantees an exception on
    // this exact, restored state. Capture transfers the raised reference.
    Err(unsafe { take_raised_exception(call_state) })
} else {
    // Transfer the owned result into the entry's Bound representation.
}
```

Taking the raised exception is a pointer transfer, not a new allocation or
incref; `_PyErr_GetRaisedException` already provides the basic operation.
The private helper also records the interpreter key. A missing exception
where the C contract requires one becomes `SystemError` through authorized
reporting, never an unchecked null owning handle. Error creation or reporting
that itself fails must preserve an appropriate CPython exception such as
`MemoryError`; it cannot assume exception construction is infallible.

At the generated boundary, keep a successful result or owned error alive
through conversion and cleanup, then commit the C result. `Ok` requires a
clear error indicator; otherwise release the result through authorized
cleanup and return an appropriate error sentinel. For `Err`, first check its
interpreter key without touching the exception pointer, then restore/steal its
reference into the current state and return the ABI's error sentinel. A
foreign error is queued under its original key and replaced with a local
`SystemError`. This catches even an error obtained indirectly from another
interpreter's callback.

Restoring or replacing an ambient exception may decref the previous exception
and reenter, so it requires the exclusive-call protocol too. Finish cleanup
before the final restore where possible; any subsequent release processing
must save and restore that incoming exception. Custom exception types stored
as `Py<PyType>` require checked binding before use. Built-in exception
accessors obtain an appropriately associated handle for the current context.
Native identity, entry, detachment, and interoperation errors never implicitly
consult ambient attachment; their explicit conversion requires an available
mutable context or private authorized boundary reporting.

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

The unsafe form lends `&mut [MaybeUninit<u8>]` and requires every byte to be
initialized before returning `Ok(())`. Error and panic paths may leave it
partially initialized: private cleanup discards the unpublished object
without reading those bytes or exposing them to Python. Initialization runs
under the exclusive-call protocol, with no context passed to the closure;
restoration and cleanup must also work when it panics. Even for zero length,
an implementation must not lend mutable access to a shared bytes singleton.

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

Both require exclusive context access and are fallible because they may
allocate or invoke Python protocols. Calling a trait method does not itself
suspend the context: its implementation receives an available capability and
only private C wrappers suspend it around actual calls. Their shared invariant
`'py` lifetime prevents scoped results from escaping or being mixed across
contexts. `FromPyObject` preserves the owner's `'a`
lifetime, allowing zero-refcount borrowed arguments; retaining one requires
`to_owned(ctx)`. Implementations returning a Rust reference with lifetime
`'view` into Python storage require both `'a: 'view` and `'py: 'view`; an
owner lifetime alone is insufficient. An implementation accepting `Py<T>`
first performs checked binding and maps mismatch using the supplied context.
The trait does not carry the short borrow lifetime of `ctx`, so it cannot
return a view requiring continuous attachment. Any such future accessor must
borrow the context for the returned view's lifetime and separately prove its
aliasing and concurrency requirements.

Exact, published `PyBytes` has a simpler contract: its initialized byte
storage is immutable and can be read while detached. Its accessor is a leaf,
and bounds the slice by the object owner and interpreter entry, without tying
it to the short context borrow:

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

The checked marker must establish exact immutable bytes storage, not merely
accept an arbitrary buffer exporter. The owner keeps the allocation live and
the entry lease protects it through detachment and interpreter shutdown;
neither protection can end while the slice is in use. The accessor performs
no incref and introduces no input copy:

```rust
let input = data.as_bytes(ctx);
let output = ctx.detach(|| compress_without_python(input))
    .map_err(|err| err.raise(ctx))?;
```

A slice into writable or externally mutable buffer storage cannot use this
proof. Such exporters require the separate buffer design in the appendix.

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
to propagate `PyErr`; only an `Ok(T)` value is converted.


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
state type and interpreter association. It is invariant in `'py` through its
borrowed module handle. Static definitions describe ABI/layout and contain no
interpreter-owned Python objects; each runtime module instance has its own
state, heap types, and custom exceptions. Instances in the same interpreter
also remain distinct: an interpreter key does not identify a module instance.

Modules always have a typed, per-module state, even if it is empty:

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

`Send + Sync` supports concurrent access in free-threaded builds. Traced
Python fields use `GcSlot<T>`, which owns an optional Python reference and
has a private interpreter key fixed at construction. GC clearing permanently
closes a slot to replacement, so reentrant code cannot repopulate an already
cleared edge while later clear callbacks are no-ops:

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
    ) -> Result<Option<Bound<'py, T>>, GcSlotError>;
}
```

`empty` is a metadata-only leaf. `get` and `replace` conservatively require
exclusive context access because their synchronization may need to park the
thread; any suspension occurs before acquiring slot protection. Both check
the slot's key before accessing its object pointer.
`replace` also binds the incoming value to the same scope through its type.
Its native `GcSlotError` distinguishes interpreter mismatch from a cleared
slot; either error leaves the slot unchanged and drops the supplied value by
the ordinary release policy. `get` returns `None` for a cleared slot after
checking identity. On success `replace` returns ownership of the removed
reference, so its decref occurs after releasing internal synchronization.
`get` acquires its returned strong reference while the slot contents are
protected, paying an incref to keep concurrent replacement safe. No API
exposes a mutable reference to the stored handle or lets safe code change a
published slot's interpreter association or reopen a cleared slot.

`#[derive(ModuleStateGc)]` emits the associated policy. A sealed field trait
recurses through `GcSlot<T>` and explicitly supported containers whose safe
mutations preserve that association. Before publishing state, `validate`
checks every traced slot against the containing module's key, including
empty slots. Bare `Py<T>`, `Option<Py<T>>`, arbitrary `Mutex`/`Arc` wrappers,
and containers exposing unchecked replacement do not automatically qualify
as derived GC fields. Rust-only state emits `ENABLED = false` and installs no
payload GC callbacks. Manual policies are unsafe and must enforce the same
publication and mutation rules, correct edge multiplicity, pointer liveness
during visits, partial-clear validity, and nonblocking traversal.

`GcSlot` clearing uses shared access and synchronized removal, never a Rust
`&mut S` across a decref. Its private implementation must let traversal visit
a live, correctly accounted reference throughout each `visitproc` call even
when another thread could otherwise mutate or clear the slot. Traversal may
rely on CPython's GIL or stop-the-world exclusion only after auditing every
supported traversal caller and ensuring slot mutations participate in that
exclusion. It must not block or invoke Python to acquire protection, expose
an unsynchronized `&Py<T>`, or hold a lock across decref. Atomic pointer loads
alone do not prove these requirements. Removed references are released only
after the slot is empty and closed and its internal protection has been
exited; clearing an already closed slot is a no-op. Ordinary replacement with
`None` merely empties a slot and does not close it. Reentrant traversal
therefore sees valid shared state. Any supported container needs the same
proof before a sealed implementation is added.

CPython allocates `StateStorage<S>`, an interpreter key, atomic phase, and
`MaybeUninit<S>`, before Rust constructs `S`. Generated glue initializes the
key from the current context. The macro rejects types whose alignment exceeds
the allocator's documented guarantee and checks all size arithmetic.
Repeated, reentrant, and concurrent exec calls require these explicit phases:

| Operation | Allowed source phase | Published phase and access |
| --- | --- | --- |
| Claim exec | `Uninitialized` | `Initializing`; any other source raises `SystemError`. |
| Finish `S::new` | `Initializing` | Validate traced associations before publication; on failure drop the unpublished value and publish `FailedWithoutValue`; otherwise write `S` and publish `Executing`. |
| Finish exec | `Executing` | `Ready` or `FailedWithValue`; `S` remains live. |
| Read state | `Ready` | Check storage identity; borrow `&S` for the intersection of the module-owner lifetime and guarded entry scope. |
| Traverse | `Executing`, `Ready`, `FailedWithValue`, `Clearing`, `Cleared`, `ClearFailed` | Borrow `&S`; never traverse `Dropping` or `Dropped`. |
| Clear | `Ready` or `FailedWithValue` | Enter `Clearing`, clear slots through `&S`, then publish `Cleared` or `ClearFailed`; later clears are no-ops. |
| Free with value | `Executing`, `Ready`, `FailedWithValue`, `Clearing`, `Cleared`, or `ClearFailed` | Enter `Dropping`, drop once, then publish `Dropped`. |
| Free without value | `Uninitialized` or `FailedWithoutValue` | Publish `Dropped` without calling `drop_in_place`. |

The exec hook has the typed signature:

```rust
type ModuleExec<S> = for<'a, 'py> fn(
    &mut ThreadContext<'py>,
    ModuleRef<'a, 'py, S>,
    &S,
) -> PyResult<()>;
```

Non-panicking guards apply failure phases during unwinding. Stores that
publish an initialized value or completed transition use release ordering;
dependent reads use acquire ordering.

Unreachability alone does not justify an exclusive Rust payload borrow:
decref may reenter traversal and free-threaded access needs synchronization.
Derived clear removes edges through their shared `GcSlot` interface before
releasing them in the containing interpreter. Traversal retains its restricted
visitor capability; it cannot acquire a general context, drain a release
queue, or bind arbitrary `Py<T>` handles. Identity validation has already
occurred at publication and mutation, rather than during GC.

Free never retries a panicking destructor. Non-panicking guards still publish
`Dropped`; Rust drops `S` but CPython retains responsibility for freeing the
module-state allocation.

Generated modules declare `Py_mod_gil = Py_MOD_GIL_NOT_USED`, the internal
exact-version ABI, the GIL/free-threaded build mode, and
`Py_mod_multiple_interpreters = Py_MOD_PER_INTERPRETER_GIL_SUPPORTED`.
These declarations require an audit of native dependencies and mutable
process-global state as well as the Rust wrapper. Process-global Rust caches
may contain only Rust data or opaque, tagged handles checked before use;
interpreter-owned runtime types and exceptions belong in module state. The
module flags advertise support but do not substitute for the identity and
lifetime checks, even when the importer disables extension checks.


#### Function definition

Module-scoped, Python-callable Rust functions remain ordinary Rust functions with two
explicit leading parameters: the thread context and the module state.
Python-visible arguments follow them. This interim example accepts exact bytes
pending a buffer API design:

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

The macro identifies context and state by position, then emits compiler-checked
uses of their declared types. The state determines `ModuleMethodDef<S>`.
Functions that create objects, convert generic values, report Python errors,
or otherwise use exclusive operations take `&mut ThreadContext`. A function
whose body uses only leaf operations may take a shared borrow. Generated
argument and result conversion still use exclusive access before and after
that body's borrow. State and payload references remain shared in either case.

`#[pyfunction]` preserves the Rust function body and emits its typed definition,
argument specification, and `METH_FASTCALL | METH_KEYWORDS` trampoline. The
trampoline enters panic containment and `with_current`, validates scoped C
inputs, recovers ready state, binds arguments, invokes Rust, and converts the
result to C before `'py` ends.

The C input is untyped, so private glue validates it against the selected
`ModuleDef<S>` before constructing a callback-scoped `ModuleRef<S>`. Methods
use the same design with `METH_METHOD`.

Panic containment covers entry bookkeeping, parsing, user code, conversion,
and cleanup. The private entry guard stays alive during panic payload
handling and error reporting; catching outside `with_current` alone would
release lifetime protection too soon. A pointer-returning boundary is
conceptually:

```rust
// SAFETY: callers must obey PyCFunctionFastWithKeywords, provide live
// callback arguments, and invoke this function with an attached thread state.
unsafe extern "C" fn trampoline(
    /* existing PyCFunctionFastWithKeywords arguments */
) -> *mut ffi::PyObject {
    let boundary = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: callback inputs satisfy entry's attached-state contract.
        let entered = unsafe {
            with_current(|ctx| {
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
            })
        };
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

`panic_to_owned_error` disposes of the payload while holding exclusive context
access and suspending it around arbitrary payload destructor code. Because a
`panic_any` payload destructor may itself panic, disposal has a second catch;
a second panic aborts. The helper then constructs a fixed-message `SystemError`
through the ordinary error path, with the context available again. A fixed
message does not make reporting allocation-free or callback-free: exception
creation can allocate, and replacing an exception can run finalizers.

Mandatory registration, restoration, and lifecycle guards must not unwind.
A panic already in flight restores attachment before the inner catch receives
it. The outer catch is a last-resort fatal boundary, not the normal path for
reporting author panics. Cleanup drain failures and inability to maintain the
CPython error contract cannot escape the non-unwinding C ABI.

Not every callback ABI can report a normal Python exception, so generated glue
uses a callback-specific policy:

| Callback kind | Panic policy |
| --- | --- |
| Function, method, getter, setter, and constructor | Set a fixed `SystemError` and return the ABI's `NULL` or `-1` error sentinel. |
| Module create | Set a fixed `SystemError` and return `NULL`. |
| Module exec | Commit the appropriate failed state phase, set a fixed `SystemError`, and return `-1`. |
| Module or instance clear | Finish the lifecycle transition, set `SystemError`, and return failure; CPython reports it as unraisable where required. |
| Traverse | Preserve incoming exception state and propagate the visitor's exact nonzero return and any error it sets. A Rust panic is fatal because continuing with an incomplete reachability report can violate GC invariants. |
| `tp_dealloc` and module free | Run mandatory non-panicking cleanup guards first, then report the panic as unraisable when an attached state is available. Abort if cleanup itself cannot be completed safely. |

Before `drop_in_place<C>`, deallocation commits the destroyed phase and
installs guards for the Python allocation and heap-type reference. A panic
therefore cannot cause a second Rust drop or skip mandatory cleanup. Module
state uses the same pattern. Generated lifecycle glue holds exclusive context
access and suspends the record around arbitrary payload/state destructors
and any C cleanup that may reenter. These destructors receive no context, so
they cannot be responsible for calling `ctx.interop` themselves. Restore the
exact state and resume the context before reporting a panic. The record,
release sink, and lifetime protection remain live throughout this cleanup.

During finalization, the same suspension preserves cleanup-phase restrictions;
it does not authorize unrestricted foreign interoperation. Only transitions
covered by the private cleanup authorization may proceed, with exact-state
restoration and protection through the full synchronous call. This applies
to panic payload destruction as well as class and module destructors.


#### Class definition and method definition

A Rust payload type `C` identifies a class layout and static definition, its
module-state type, and its GC policy. Each module instance creates its own
runtime Python type from that definition:

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
        .methods(&ENCODER_METHODS)
        .getsets(&ENCODER_GETSETS);
```

This prevents safe code from combining an unrelated payload, module state, or
definition. `#[derive(PyClassGc)]` uses the same sealed field rules as module
state. `ENABLED` controls payload-edge handling; every generated instance of
a heap type still owns an implicit edge to its actual type, which may in turn
own its defining module. Generated traversal reports that edge even for a
Rust-only payload such as `Encoder`. Manual payload policies are unsafe
because they must validate every edge's association, report it with the
correct multiplicity, clear it safely, and obey the class's synchronization
rules. The common glue handles the implicit type edge separately.

`PyType<C = UntypedClass>` and `PyInstance<C: PyClass>` are transparent
markers over this identity. The default represents an ordinary untyped Python
type object.

`ENCODER_CLASS` creates `Bound<PyType<Encoder>>`; a traced module-state field
stores it in `GcSlot<PyType<Encoder>>`. Upcasts to untyped `PyType` or `PyAny`
do not touch the refcount or change interpreter association. Downcasts first
establish the correct context and then validate the class token. Generated
methods, getsets, and constructors all carry `C`; method state is fixed to
`C::State`, but its value comes from the actual defining module instance.

`ClassDef<C>` computes the payload layout with checked arithmetic and installs
lifecycle slots. Types whose alignment exceeds the object allocator's
documented guarantee are rejected before allocation. The allocation records
initialization, clear, and drop phases so callbacks never read uninitialized
or destroyed `C`.

`ClassDef<C>::create(ctx, ModuleRef<C::State>)` combines static slots with the
runtime module pointer in the same invariant scope and returns
`Bound<PyType<C>>`. It verifies the unique `C::definition()`; a second static
definition requires a Rust newtype. The definition address is also stored as
`Py_tp_token`. It proves layout identity, not interpreter identity or a unique
runtime type: several module instances, including in different interpreters,
can have types with that token. No process-global Python type cache is used.

`type_object.new_instance(ctx, payload)` validates the type token and every
payload GC slot's interpreter association before publishing an instance,
then returns `Bound<PyInstance<C>>`. Failure drops the unpublished payload
using the ordinary release policy, which is safe even if it contains foreign
handles. Generated methods recover the defining module through the runtime
type's module association (`METH_METHOD` where applicable), validate its state
definition and interpreter key, and cap payload/state borrows by both owner
and entry lifetimes.

Rust structs in classes must be `Send + Sync + 'static`. Python methods receive only `&self`,
never `&mut self`, because aliases can call the same object concurrently in a
free-threaded interpreter. Mutable payload state therefore requires interior
mutability. The rules for a blocking interpreter-aware lock remain an open
decision listed in the appendix; an ordinary `Mutex` must not be presented as
the general solution until those rules are settled.


Python-owning fields add payload edges to the generated GC slots:

```rust
#[derive(PyClassGc)]
struct Node {
    parent: GcSlot<PyInstance<Node>>,
    value: u64,
}
```

Generated heap-type instances, including both `Encoder` and `Node`, set
`Py_TPFLAGS_HAVE_GC` and install traversal for the actual heap-type edge;
`Node` additionally traverses and clears its payload edge. Lifecycle rules are:

| Operation | Invariant |
| --- | --- |
| Allocate | Validate payload associations against the type/context key; write `C` into an untracked object, publish ready, then call `PyObject_GC_Track`. |
| Traverse | Visit the actual heap type and payload edges through shared access; only call `visitproc`, propagate its exact return, and never invoke Python or block independently of the visitor. |
| Clear | Remove payload edges through synchronized `GcSlot` access using `&C`; release removed references after leaving slot protection; later clears are no-ops. The instance retains its type until deallocation. |
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

All generated slots retain CPython's existing ABI and panic containment.
Executable callbacks enter the common context machinery, including authorized
cleanup entry during finalization; traversal uses only the restricted visitor
capability. Runtime type references belong in module state using its traced
field policy; `add_type` accepts the safe untyped upcast.


#### Argument parsing

Argument parsing exposes a small PyO3-like signature syntax:

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

A new `xxtestrustapi` module will be introduced to the normal Python test suite
to ensure the correctness of the implementation and check invariants of the
Rust code. The design requires the following focused coverage before enabling
its support declarations:

- Compile-fail cases for mixing invariant scopes, returning scoped handles
  or Python-backed Rust references from an entry, treating a static `Py<T>`
  owner as permission for static payload access, and using a context captured
  directly or through scoped TLS while that context is exclusively borrowed.
  Include attachment-dependent views held across detach, protocol calls,
  explicit decref, and release drains. Compile-pass cases must allow opaque
  handles and non-`Send` Rust captures/results in same-thread `detach`, and
  exact immutable bytes views whose owner and entry outlive detachment.
- Regressions modeled on PyO3 #2141 and #3640: `SendWrapper`, a capture-free
  function reading scoped TLS, and dynamic exclusive TLS/`RefCell` borrowing.
  Check implicit formatting, comparison, conversion, iteration, and destruction
  as well as explicit methods. A handle reachable while detached must remain
  opaque without an available context.
- Interpreter A -> Python protocol -> interpreter B -> Rust callback reentry,
  including a TLS hook holding A's values. Repeat with a different thread
  state in A. Old contexts stay inaccessible; callback scopes use the actual
  state, and return/unwind restores the exact outer state. An extensible
  conversion trait must receive an available context, not one whose record
  was suspended around the whole trait invocation.
- Cooperating foreign wrappers under `interop`, allowed nested detachment,
  and native preflight refusal with unchanged registration and attachment.
  Subprocess tests for uncooperative public and internal detach/swap calls
  must fail before invalidating an available context. Include direct Rust TLS
  access from the detached closure, without a generated callback entry.
  Forgetting public handles must not affect registration or lease ownership.
- Two simultaneous subinterpreters, including separate-GIL configurations:
  distinct module state, runtime classes, and exceptions; checked binding,
  conversion, explicit destruction, and traced-field publication/mutation
  reject foreign handles before reading their pointers. Repeat with multiple
  module instances in one interpreter and with extension import checks off.
- Legitimately shared constants obtained independently in two interpreters:
  pointer equality or immortality does not permit relabeling a stored handle.
  Interpreter destruction and runtime reinitialization cannot revive old
  keys, even if allocator addresses or public interpreter IDs are reused.
- Attached, detached, wrong-interpreter, and unwinding drops: one ownership
  transfer and at most one decref per handle, correct interpreter on release,
  and preservation of any pending exception. Include TLS/native-thread drops
  after finalization and `Py::drop_with` mismatch behavior. An implicit drop
  while a shared context is exposed must never run a finalizer synchronously.
  Exercise inline sink overflow, nested scopes, and suspended outer sinks;
  no node or lookup may retain a pointer to a retired entry.
- Fault-injected queue allocation failure, full ordinary pending-call queues,
  and admission/notification races with shutdown: accepted requests drain
  before teardown; rejected requests neither dereference nor decref objects.
  Include producers paused at each admission boundary and finalizers that
  reenter Rust or enqueue further releases, without locks held across Python.
  After external admission closes, authorized cleanup drops still drain;
  unauthenticated producers cannot acquire cleanup permission. Test cascades
  of queued drops from module/type teardown after the first final drain.
  Include an active consumer with an empty queue whose C deallocator detaches:
  its batch and interpreter must remain protected until draining completes.
- Shutdown racing with an active detached Rust scope: its state remains live
  through reattachment; cleanup reentries after normal admission closes work
  and cannot request explicit detach. Test audited internal temporary detach
  during cleanup as well. Repeat cross-extension reentry with two `cdylib`s
  to verify CPython-owned OS-thread registrations and lifetime bookkeeping.
- Panic during detachment, parsing, conversion, and payload destruction:
  exact attachment is restored, the entry lease survives panic reporting,
  and a second panic or failed restoration cannot cross the C ABI. Include
  finalizers triggered by replacing exceptions and allocation failure during
  reporting. Owned errors caught and discarded in Rust leave no ambient error;
  same-interpreter state transfer works and foreign errors are rejected.
- Fork with queued references and paused detached producers, including
  surviving-thread scope restoration and child-side interpreter deletion:
  no inherited lock, orphaned in-flight count, or stale scope record prevents
  progress or allows access to a removed interpreter.
- Concurrent GC-slot reads, replacement, traversal, and clear: returned owned
  references remain live, foreign slots cannot be published, reentrant visits
  never overlap an exclusive payload borrow, closed slots reject replacement,
  and heap-type/module cycles are collected even for Rust-only payloads.

Run the relevant runtime cases on GIL and free-threaded builds. Also verify
that shared Rust payload references cannot expose unsynchronized interior
mutation, even when attachment is released and another thread calls Python.

Measure leaf access, unbound binding, outermost and nested entry, exclusive
call suspension/resumption, GC-slot access, explicit decref, inline scoped
drops, and interpreter-queue overflow against equivalent C operations.
Include contention, tiny frequent callbacks, compression throughput, and
retained-memory measurements. The intended cost distribution is:

- Attachment and phase checks at scope entry and transition boundaries,
  with no repeated TLS validation for consecutive leaf operations.
- Per-OS-thread registration changes around potentially reentrant calls;
  no global registry lookup or allocation on the ordinary suspension path.
- Key comparisons at unbound-handle and traced-field publication boundaries,
  and finalization admission bookkeeping at protected entry boundaries.
- Inline enqueue for ordinary scoped drops, batched decref, and native queue
  synchronization/allocation on overflow or unbound submissions without a
  matching sink. Explicit release avoids that queue traffic.
- Refcount traffic for mutable GC-slot reads; no copy for exact immutable
  bytes input or fresh allocation merely to capture an existing C exception.

These are implementation targets, not measured performance guarantees. Tune
batch sizes and sink storage using those measurements before claiming parity
with equivalent C code.

CI will check that every Python-touching safe `c_api` wrapper accepts a
context or sealed visitor/release capability, and that shared-context
wrappers have an audited leaf contract. Private capability constructors,
transition gates, restoration, and exception paths need separate audits;
a signature check cannot establish those proofs. Operations such as deferred
submission that use neither need an audited thread-state-free justification,
including their allocation, notification, and failure paths.

### Appendix: Open decisions and future work

The following items are intentionally kept out of the normative API sections:

- **Reclamation after interpreter shutdown.** The normative policy safely
  abandons references that cannot be submitted before shutdown or on queue
  allocation failure. Registered, revocable ownership records and reserved
  release storage could strengthen reclamation guarantees, at a cost to
  handle construction, memory usage, and synchronization.
- **Buffer protocol.** No safe `PyBuffer` API is included until the design
  distinguishes read-only and writable exporters; specifies contiguity,
  format, alignment, ownership, and release on every exit path; and prevents
  Rust slices from racing with Python mutation or escaping across detach and
  reentrant calls. The exact immutable bytes accessor above provides the
  initial zero-copy input path under a narrower ownership contract.
- **Interpreter-aware locking.** The design must either provide a lock which
  cooperates with attachment, the GIL, and free-threaded stop-the-world pauses,
  or prohibit blocking, detaching, calling Python, and reaching safepoints
  while an ordinary Rust lock guard is held. Poisoning and traversal behavior
  also require an explicit policy.
- **Additional GC containers.** Each container must prove interpreter
  association at publication and mutation, correct edge accounting, safe
  concurrent traversal/clear, and nonblocking visitor access before gaining a
  sealed implementation. Guarded borrowed slot reads may avoid `get`'s incref
  if their reentrancy and lifetime requirements can be expressed safely.
- **Explicit-state C optimization.** Benchmark explicit-`PyThreadState *`
  variants for TLS-heavy hot paths; this would not change the safe Rust API.
- **Rust runtime and panic strategy.** The initial API requires `std` and
  `panic=unwind` so generated callbacks can contain panics, complete lifecycle
  cleanup, and report errors. A `panic=abort` build remains ABI-safe but cannot
  provide that recovery behavior. Any future `no_std` configuration must use
  panic-abort or provide another complete no-unwind boundary.
