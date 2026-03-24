/*
 * Fallback <stdatomic.h> for bindgen.
 *
 * Some libclang builds ship a <stdatomic.h> that silently provides no
 * declarations.  This makes any header that relies on C11 atomics
 * (e.g. mimalloc) unparseable by bindgen.
 *
 * This file provides the standard C11 atomics API with non-atomic stubs.
 * Bindgen only needs correct type layouts -- it never executes atomic
 * operations -- so plain loads/stores are sufficient.
 *
 * The build script adds this directory via -isystem so it shadows a
 * broken system header while remaining invisible when the real header
 * works (libclang picks whichever it finds first on the include path).
 */
#ifndef _STDATOMIC_BINDGEN_FALLBACK_H
#define _STDATOMIC_BINDGEN_FALLBACK_H

/* Strip _Atomic qualifier -- bindgen cares about layout, not atomicity. */
#define _Atomic(tp)  tp

typedef enum {
    memory_order_relaxed,
    memory_order_consume,
    memory_order_acquire,
    memory_order_release,
    memory_order_acq_rel,
    memory_order_seq_cst
} memory_order;

#define ATOMIC_VAR_INIT(value)  (value)

#define atomic_load_explicit(p, mo)                              (*(p))
#define atomic_store_explicit(p, v, mo)                          ((void)(*(p) = (v)))
#define atomic_fetch_add_explicit(p, v, mo)                      (*(p))
#define atomic_fetch_sub_explicit(p, v, mo)                      (*(p))
#define atomic_fetch_or_explicit(p, v, mo)                       (*(p))
#define atomic_fetch_and_explicit(p, v, mo)                      (*(p))
#define atomic_compare_exchange_weak_explicit(p, e, d, s, f)     1
#define atomic_compare_exchange_strong_explicit(p, e, d, s, f)   1
#define atomic_exchange_explicit(p, v, mo)                       (*(p))
#define atomic_thread_fence(mo)                                  ((void)0)

#endif /* _STDATOMIC_BINDGEN_FALLBACK_H */
