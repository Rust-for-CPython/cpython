use std::env;

/// Print necessary link arguments for the library depending on the build
/// configuration (static or shared)
pub fn print_linker_args() {
    println!("cargo:rerun-if-env-changed=RUST_SHARED_BUILD");
    println!("cargo:rerun-if-env-changed=BLDSHARED_EXE");
    println!("cargo:rerun-if-env-changed=BLDSHARED_ARGS");
    println!("cargo:rerun-if-env-changed=LIBPYTHON");
    println!("cargo:rerun-if-env-changed=PY_CC");
    println!("cargo:rerun-if-env-changed=PYTHON_BUILD_DIR");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let shared_build =
        env::var("RUST_SHARED_BUILD").unwrap_or_default() == "1" || target_os == "ios";

    // On Apple platforms (macOS, iOS), Cargo's cdylib produces a Mach-O
    // dynamiclib (via -dynamiclib), but CPython's C extensions are built as
    // bundles (via -bundle). Unlike bundles, dynamiclibs require all symbols
    // to be resolved at link time. Pass -undefined dynamic_lookup so that
    // Python C API symbols are resolved at load time by the interpreter.
    if target_os == "macos" || target_os == "ios" {
        println!("cargo:rustc-cdylib-link-arg=-undefined");
        println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
    }

    // Apple framework builds for iOS encode framework search/link flags in
    // BLDSHARED_EXE. Skip the linker executable itself and filter out flags
    // that conflict with Cargo's own cdylib invocation.
    if shared_build && let Ok(args) = env::var("BLDSHARED_EXE") {
        print_link_args(&args, true);
    }

    // Pass platform-specific shared link arguments (e.g. PY_CORE_LDFLAGS)
    // from the CPython build system.
    if let Ok(args) = env::var("BLDSHARED_ARGS") {
        print_link_args(&args, false);
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

fn print_link_args(raw: &str, skip_first: bool) {
    let mut args = shlex::split(raw).expect("Invalid linker args");
    if skip_first {
        args = strip_linker_executable(args, env::var("PY_CC").ok().as_deref());
    }

    let build_dir = env::var("PYTHON_BUILD_DIR").ok();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // -bundle_loader only works with -bundle, not with Cargo's
            // cdylib mode (-dynamiclib).
            "-bundle_loader" => {
                i += 2;
            }
            // Cargo chooses the Mach-O output type for cdylib already.
            "-bundle" | "-dynamiclib" => {
                i += 1;
            }
            // dynamic_lookup is added explicitly for Apple targets above.
            "-undefined" if i + 1 < args.len() && args[i + 1] == "dynamic_lookup" => {
                i += 2;
            }
            "-F" if i + 1 < args.len() => {
                let value = if args[i + 1] == "." {
                    build_dir.clone().unwrap_or_else(|| ".".to_string())
                } else {
                    args[i + 1].clone()
                };
                println!("cargo:rustc-cdylib-link-arg=-F");
                println!("cargo:rustc-cdylib-link-arg={value}");
                i += 2;
            }
            _ => {
                println!("cargo:rustc-cdylib-link-arg={}", args[i]);
                i += 1;
            }
        }
    }
}

// BLDSHARED_EXE may start with a wrapper/compiler command such as
// "xcrun --sdk iphoneos clang" or "ccache clang". Strip that prefix so only
// linker flags are forwarded to rustc.
fn strip_linker_executable(args: Vec<String>, compiler: Option<&str>) -> Vec<String> {
    if let Some(compiler) = compiler
        && let Some(prefix) = compiler_command_prefix(compiler)
        && args.as_slice().starts_with(prefix.as_slice())
    {
        return args[prefix.len()..].to_vec();
    }

    if args.is_empty() {
        return args;
    }
    args[1..].to_vec()
}

fn compiler_command_prefix(raw: &str) -> Option<Vec<String>> {
    let tokens = shlex::split(raw)?;
    let last_non_flag = tokens.iter().rposition(|token| !token.starts_with('-'))?;
    Some(tokens.into_iter().take(last_non_flag + 1).collect())
}
