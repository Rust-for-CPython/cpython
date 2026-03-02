use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let srcdir = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("expected Modules/cpython-sys to live under the source tree");
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    let builddir = env::var("PYTHON_BUILD_DIR").ok();
    if gil_disabled(srcdir, builddir.as_deref()) {
        println!("cargo:rustc-cfg=py_gil_disabled");
    }
    println!("cargo::rustc-check-cfg=cfg(py_gil_disabled)");
    generate_c_api_bindings(srcdir, builddir.as_deref(), out_path.as_path());
}

fn gil_disabled(srcdir: &Path, builddir: Option<&str>) -> bool {
    let mut candidates = Vec::new();
    if let Some(build) = builddir {
        candidates.push(PathBuf::from(build));
    }
    candidates.push(srcdir.to_path_buf());
    for base in candidates {
        let path = base.join("pyconfig.h");
        if let Ok(contents) = std::fs::read_to_string(&path)
            && contents.contains("Py_GIL_DISABLED 1")
        {
            return true;
        }
    }
    false
}

fn generate_c_api_bindings(srcdir: &Path, builddir: Option<&str>, out_path: &Path) {
    let mut builder = bindgen::Builder::default().header("wrapper.h");

    // Suppress all clang warnings (deprecation warnings, etc.)
    builder = builder.clang_arg("-w");

    // Tell clang the correct target triple for cross-compilation.
    // LLVM_TARGET is the clang/LLVM triple which may differ from the Rust
    // target (e.g. arm64-apple-macosx vs aarch64-apple-darwin, or
    // riscv64-unknown-linux-gnu vs riscv64gc-unknown-linux-gnu).
    // Falls back to Cargo's TARGET if LLVM_TARGET is not set.
    let target = env::var("LLVM_TARGET")
        .or_else(|_| env::var("TARGET"))
        .unwrap_or_default();
    if !target.is_empty() {
        builder = builder.clang_arg(format!("--target={}", target));
    }

    // Extract cross-compilation flags from the C compiler command (PY_CC)
    // and preprocessor flags (PY_CPPFLAGS). These provide the sysroot and
    // include paths that bindgen's clang needs to find system headers when
    // cross-compiling.
    //
    // - WASI: the sysroot is embedded in CC ("clang --sysroot=...")
    // - iOS: -isysroot in CPPFLAGS points to the SDK
    let mut have_sysroot = false;
    for env_name in ["PY_CC", "PY_CPPFLAGS"] {
        if let Ok(value) = env::var(env_name) {
            if let Some(flags) = shlex::split(&value) {
                let mut iter = flags.iter().peekable();
                while let Some(flag) = iter.next() {
                    if flag.starts_with("--sysroot")
                        || flag.starts_with("-isysroot")
                    {
                        builder = builder.clang_arg(flag);
                        have_sysroot = true;
                        // Handle "-isysroot <path>" (space-separated)
                        if flag == "-isysroot" || flag == "--sysroot" {
                            if let Some(path) = iter.next() {
                                builder = builder.clang_arg(path);
                            }
                        }
                    } else if flag.starts_with("-I")
                        || flag.starts_with("-D")
                        || flag.starts_with("-isystem")
                    {
                        builder = builder.clang_arg(flag);
                    }
                }
            }
        }
    }

    // Android NDK: the cross-compiler binary knows its own sysroot
    // implicitly, but bindgen's libclang does not. The NDK sysroot is
    // at .../toolchains/llvm/prebuilt/<host>/sysroot, which is a sibling
    // of the bin/ directory containing the compiler.
    if !have_sysroot && target.contains("android") {
        if let Ok(cc) = env::var("PY_CC") {
            if let Some(parts) = shlex::split(&cc) {
                if let Some(binary) = parts.first() {
                    let cc_path = Path::new(binary);
                    if let Some(bin_dir) = cc_path.parent() {
                        let sysroot = bin_dir.with_file_name("sysroot");
                        if sysroot.is_dir() {
                            builder = builder.clang_arg(format!(
                                "--sysroot={}",
                                sysroot.display()
                            ));
                        }
                    }
                }
            }
        }
    }

    // Always search the source dir and the public headers.
    let mut include_dirs = vec![srcdir.to_path_buf(), srcdir.join("Include")];
    // Include the build directory if provided; out-of-tree builds place
    // the generated pyconfig.h there.
    if let Some(build) = builddir {
        include_dirs.push(PathBuf::from(build));
    }

    for dir in include_dirs {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }

    let bindings = builder
        .allowlist_function("_?Py.*")
        .allowlist_type("_?Py.*")
        .allowlist_var("_?Py.*")
        .blocklist_type("^PyMethodDef$")
        .blocklist_type("PyObject")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/c_api.rs file.
    bindings
        .write_to_file(out_path.join("c_api.rs"))
        .expect("Couldn't write bindings!");
}
