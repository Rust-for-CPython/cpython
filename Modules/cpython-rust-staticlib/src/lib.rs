// Re-export each Rust module's PEP 793 export hook so its object code (and
// the `PyModExport_<name>` symbol) is retained in the static archive.
//
// NOTE: builtin/static interpreter builds currently register modules through
// `struct _inittab`, which only carries `PyInit_*`-style functions — slots-
// only (PyModExport) modules can be imported as shared extensions today, but
// wiring them into a static interpreter needs PEP 793 inittab support in the
// C import machinery first.
pub use _base64::PyModExport;
