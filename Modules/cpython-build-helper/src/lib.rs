use std::env;

/// Print necessary link arguments for the library depending on the build
/// configuration (static or shared)
pub fn print_linker_args() {
    let target = env::var("TARGET").unwrap_or_default();

    // On Apple platforms (macOS, iOS), Cargo's cdylib produces a Mach-O
    // dynamiclib (via -dynamiclib), but CPython's C extensions are built as
    // bundles (via -bundle). Unlike bundles, dynamiclibs require all symbols
    // to be resolved at link time. Pass -undefined dynamic_lookup so that
    // Python C API symbols are resolved at load time by the interpreter.
    if target.contains("apple") {
        println!("cargo:rustc-cdylib-link-arg=-undefined");
        println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
    }

    // Pass platform-specific shared link arguments (e.g. PY_CORE_LDFLAGS)
    // from the CPython build system.
    if let Ok(args) = env::var("BLDSHARED_ARGS") {
        let args = shlex::split(&args).expect("Invalid BLDSHARED_ARGS");
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            // -bundle_loader is incompatible with Cargo's cdylib on macOS
            // (it only works with -bundle, not -dynamiclib). Skip it and
            // its argument.
            if arg == "-bundle_loader" {
                iter.next(); // skip the path argument
                continue;
            }
            println!("cargo:rustc-cdylib-link-arg={}", arg);
        }
    }

    // On Android (and Cygwin), extension modules must link against libpython.
    // LIBPYTHON is set by the CPython build system on these platforms and
    // typically contains "-L. -lpython3.X". The "-L." is relative to the
    // make build directory, so resolve it to an absolute path using
    // PYTHON_BUILD_DIR.
    if let Ok(libpython) = env::var("LIBPYTHON") {
        let builddir = env::var("PYTHON_BUILD_DIR").ok();
        for arg in shlex::split(&libpython).expect("Invalid LIBPYTHON") {
            if arg == "-L."
                && let Some(ref dir) = builddir
            {
                println!("cargo:rustc-cdylib-link-arg=-L{}", dir);
                continue;
            }
            println!("cargo:rustc-cdylib-link-arg={}", arg);
        }
    }

    // Static linker configuration is in cpython-rust-staticlib
}
