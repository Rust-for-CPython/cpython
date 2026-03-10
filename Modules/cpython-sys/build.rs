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

    // Extract cross-compilation flags from the C compiler command (PY_CC),
    // preprocessor flags (PY_CPPFLAGS), and compiler flags (PY_CFLAGS).
    // These provide the sysroot, include paths, and defines that bindgen's
    // clang needs when cross-compiling.
    //
    // - WASI: sysroot in CC, -D_WASI_EMULATED_SIGNAL in CFLAGS
    // - iOS: -isysroot in CPPFLAGS
    let mut have_sysroot = false;
    for env_name in ["PY_CC", "PY_CPPFLAGS", "PY_CFLAGS"] {
        if let Ok(value) = env::var(env_name)
            && let Some(flags) = shlex::split(&value)
        {
            let mut iter = flags.iter().peekable();
            while let Some(flag) = iter.next() {
                if flag.starts_with("--sysroot") || flag.starts_with("-isysroot") {
                    builder = builder.clang_arg(flag);
                    have_sysroot = true;
                    // Handle "-isysroot <path>" (space-separated)
                    if (flag == "-isysroot" || flag == "--sysroot")
                        && let Some(path) = iter.next()
                    {
                        builder = builder.clang_arg(path);
                    }
                } else if flag.starts_with("-I")
                    || flag.starts_with("-D")
                    || flag.starts_with("-std=")
                    || flag.starts_with("-isystem")
                {
                    builder = builder.clang_arg(flag);
                }
            }
        }
    }

    // WASI SDK: WASI_SDK_PATH is set by Tools/wasm/wasi/__main__.py.
    // The sysroot is at $WASI_SDK_PATH/share/wasi-sysroot.
    if !have_sysroot
        && target.contains("wasi")
        && let Ok(sdk_path) = env::var("WASI_SDK_PATH")
    {
        let sysroot = PathBuf::from(&sdk_path).join("share").join("wasi-sysroot");
        if sysroot.is_dir() {
            builder = builder.clang_arg(format!("--sysroot={}", sysroot.display()));
            have_sysroot = true;
        }
    }

    // Android NDK: ANDROID_HOME is set by the CI/user environment, and
    // Android/android-env.sh sets CC to the NDK clang binary at:
    //   $ANDROID_HOME/ndk/<ver>/toolchains/llvm/prebuilt/<host>/bin/<triple>-clang
    // The sysroot is a sibling of bin/:
    //   .../toolchains/llvm/prebuilt/<host>/sysroot
    if !have_sysroot
        && target.contains("android")
        && let Ok(cc) = env::var("PY_CC")
        && let Some(parts) = shlex::split(&cc)
        && let Some(binary) = parts.first()
        && let Some(bin_dir) = Path::new(binary).parent()
    {
        let sysroot = bin_dir.with_file_name("sysroot");
        if sysroot.is_dir() {
            builder = builder.clang_arg(format!("--sysroot={}", sysroot.display()));
        }
    }

    // Include the build directory first so that cross-build pyconfig.h
    // takes precedence over any pyconfig.h in the source tree (which may
    // be from a native build with different settings like LONG_BIT).
    let mut include_dirs = Vec::new();
    if let Some(build) = builddir {
        include_dirs.push(PathBuf::from(build));
    }
    include_dirs.push(srcdir.to_path_buf());
    include_dirs.push(srcdir.join("Include"));

    for dir in include_dirs {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    builder = add_target_clang_args(builder, builddir);

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

fn add_target_clang_args(
    mut builder: bindgen::Builder,
    builddir: Option<&str>,
) -> bindgen::Builder {
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("apple-ios") {
        return builder;
    }

    // For iOS targets, bindgen may parse headers with an iOS simulator/device
    // target but without a deployment minimum, which disables TLS support.
    let deployment_target = ios_deployment_target(builddir).unwrap_or_else(|| "13.0".to_string());
    builder = builder.clang_arg(format!("-mios-version-min={deployment_target}"));
    builder
}

fn ios_deployment_target(builddir: Option<&str>) -> Option<String> {
    if let Ok(value) = env::var("IPHONEOS_DEPLOYMENT_TARGET")
        && !value.is_empty()
    {
        return Some(value);
    }

    let builddir = builddir?;
    let makefile = Path::new(builddir).join("Makefile");
    let text = std::fs::read_to_string(makefile).ok()?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("IPHONEOS_DEPLOYMENT_TARGET=") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}
