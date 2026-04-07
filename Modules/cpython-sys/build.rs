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
    emit_rerun_instructions(builddir.as_deref());
    let gil_disabled = gil_disabled(srcdir, builddir.as_deref());
    if gil_disabled {
        println!("cargo:rustc-cfg=py_gil_disabled");
    }
    println!("cargo::rustc-check-cfg=cfg(py_gil_disabled)");
    generate_c_api_bindings(
        srcdir,
        builddir.as_deref(),
        out_path.as_path(),
        gil_disabled,
    );
}

// Bindgen depends on build-time env and, on iOS, can also inherit the
// deployment target from the generated Makefile. Declare both so Cargo reruns
// the build script when those inputs change.
fn emit_rerun_instructions(builddir: Option<&str>) {
    for var in [
        "IPHONEOS_DEPLOYMENT_TARGET",
        "LLVM_TARGET",
        "PY_DEBUG",
        "PY_GIL_DISABLED",
        "PYTHON_BUILD_DIR",
        "PY_CC",
        "PY_CPPFLAGS",
        "PY_CFLAGS",
        "TARGET",
        "WASI_SDK_PATH",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    if let Some(builddir) = builddir {
        let makefile = Path::new(builddir).join("Makefile");
        println!("cargo:rerun-if-changed={}", makefile.display());
    }
}

/// Find the newest clang resource directory on the system.
///
/// Ubuntu 24.04 ships libclang-18 whose built-in headers (stdatomic.h,
/// mmintrin.h) are broken.  CI jobs install a newer clang (e.g. clang-20)
/// whose resource directory at /usr/lib/llvm-20/lib/clang/20 has working
/// headers.  We pass -resource-dir to bindgen's clang so it picks up those
/// headers instead of the broken libclang-18 ones.
fn newest_clang_resource_dir() -> Option<PathBuf> {
    // On Windows, derive the resource directory from LIBCLANG_PATH so that
    // bindgen uses headers matching the libclang.dll it loads, rather than
    // picking up a different, incompatible, system LLVM installation
    if cfg!(windows) {
        return clang_resource_dir_from_libclang_path();
    }

    let base = Path::new("/usr/lib");
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(base).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(ver_str) = name.strip_prefix("llvm-")
            && let Ok(ver) = ver_str.parse::<u32>()
        {
            // Resource dir: /usr/lib/llvm-<N>/lib/clang/<N>
            let resource_dir = entry.path().join("lib").join("clang").join(ver_str);
            if resource_dir.join("include").is_dir()
                && best.as_ref().map_or(true, |(v, _)| ver > *v)
            {
                best = Some((ver, resource_dir));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Derive the clang resource directory from LIBCLANG_PATH.
///
/// When LIBCLANG_PATH points to e.g. `...\bin`, the resource directory is
/// at `...\lib\clang\<version>`.  We pick the highest version found.
fn clang_resource_dir_from_libclang_path() -> Option<PathBuf> {
    let libclang_path = env::var("LIBCLANG_PATH").ok()?;
    let bin_dir = Path::new(&libclang_path);
    // LIBCLANG_PATH typically points to a `bin` directory; the resource
    // dir lives under the sibling `lib/clang/<version>`.
    let clang_lib_dir = bin_dir.parent()?.join("lib").join("clang");
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(&clang_lib_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Version directories can be just a major number (e.g. "18") or
        // a full dotted version (e.g. "18.1.3"). Parse the major part.
        if let Ok(ver) = name.split('.').next().unwrap_or("").parse::<u32>() {
            let resource_dir = entry.path();
            if resource_dir.join("include").is_dir()
                && best.as_ref().map_or(true, |(v, _)| ver > *v)
            {
                best = Some((ver, resource_dir));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn gil_disabled(srcdir: &Path, builddir: Option<&str>) -> bool {
    if env_var_is_truthy("PY_GIL_DISABLED") {
        return true;
    }

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

fn env_var_is_truthy(name: &str) -> bool {
    env::var(name)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn generate_c_api_bindings(
    srcdir: &Path,
    builddir: Option<&str>,
    out_path: &Path,
    gil_disabled: bool,
) {
    let mut builder = bindgen::Builder::default().header("wrapper.h");

    // Suppress all clang warnings (deprecation warnings, etc.)
    builder = builder.clang_arg("-w");

    if env_var_is_truthy("PY_DEBUG") {
        builder = builder.clang_arg("-D_DEBUG");
    }
    if gil_disabled {
        builder = builder.clang_arg("-DPy_GIL_DISABLED=1");
    }

    // Use the newest clang resource directory available on the system.
    // Bindgen links against whatever libclang it finds (often an older
    // system version), but the built-in headers in that version may be
    // broken (e.g. libclang-18 on Ubuntu 24.04 has broken stdatomic.h
    // and mmintrin.h).  Overriding -resource-dir makes clang use a
    // newer set of built-in headers without changing which libclang.so
    // is loaded.
    if let Some(resource_dir) = newest_clang_resource_dir() {
        eprintln!(
            "cpython-sys: using clang resource dir {}",
            resource_dir.display()
        );
        builder = builder.clang_arg(format!("-resource-dir={}", resource_dir.display()));
    }

    // Tell clang the correct target triple for cross-compilation when we have
    // an LLVM-specific triple. Otherwise let bindgen translate Cargo's TARGET
    // itself (e.g. aarch64-apple-ios-sim -> arm64-apple-ios-simulator).
    let cargo_target = env::var("TARGET").unwrap_or_default();
    let llvm_target = env::var("LLVM_TARGET").unwrap_or_default();
    if !llvm_target.is_empty() && llvm_target != cargo_target {
        builder = builder.clang_arg(format!("--target={llvm_target}"));
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
        && cargo_target.contains("wasi")
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
        && cargo_target.contains("android")
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

    let dll_name = python_dll_name(srcdir, env_var_is_truthy("PY_DEBUG"), gil_disabled);
    let bindings = patch_windows_imported_pointer_globals(bindings.to_string(), &dll_name);

    // Write the bindings to the $OUT_DIR/c_api.rs file.
    std::fs::write(out_path.join("c_api.rs"), bindings).expect("Couldn't write bindings!");
}

/// Build the Windows DLL base name: `python{major}{minor}[t][_d]`.
fn python_dll_name(srcdir: &Path, debug: bool, gil_disabled: bool) -> String {
    let patchlevel = srcdir.join("Include").join("patchlevel.h");
    let contents =
        std::fs::read_to_string(&patchlevel).expect("failed to read Include/patchlevel.h");

    let major = extract_define_int(&contents, "PY_MAJOR_VERSION");
    let minor = extract_define_int(&contents, "PY_MINOR_VERSION");

    let mut name = format!("python{major}{minor}");
    if gil_disabled {
        name.push('t');
    }
    if debug {
        name.push_str("_d");
    }
    name
}

fn extract_define_int(contents: &str, name: &str) -> u32 {
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#define")
            && let Some(value) = rest.trim().strip_prefix(name)
            && let Ok(n) = value.trim().parse()
        {
            return n;
        }
    }
    panic!("could not find #define {name} in patchlevel.h");
}

fn patch_windows_imported_pointer_globals(bindings: String, dll_name: &str) -> String {
    // On Windows/MSVC, exported data is imported through a synthetic
    // "__imp_<symbol>" pointer in the import address table (IAT).  A plain
    // `extern { pub static X: *mut T; }` linked via import library fails for
    // data symbols because the import library only defines `__imp_X`, not `X`.
    //
    // Using `#[link_name = "__imp_X"]` links successfully but produces a
    // single load — returning the *address* of the variable (the IAT slot
    // value) rather than the variable's value.  C's `__declspec(dllimport)`
    // generates two loads to chase the indirection.
    //
    // The fix: annotate pointer-valued extern statics with `raw-dylib` on
    // Windows so Rust generates the import thunk itself and handles the IAT
    // indirection correctly — two loads, matching `__declspec(dllimport)`.
    let lines: Vec<_> = bindings.lines().collect();
    let mut patched = String::with_capacity(bindings.len());
    let mut index = 0;

    while index < lines.len() {
        if lines[index] == "unsafe extern \"C\" {"
            && lines
                .get(index + 1)
                .and_then(|l| parse_pointer_static_decl(l))
                .is_some()
            && lines.get(index + 2).is_some_and(|l| l.trim() == "}")
        {
            patched.push_str(&format!(
                "#[cfg_attr(windows, link(name = \"{dll_name}\", kind = \"raw-dylib\"))]\n"
            ));
            // Keep the original extern block unchanged.
            for i in index..index + 3 {
                patched.push_str(lines[i]);
                patched.push('\n');
            }
            index += 3;
            continue;
        }

        patched.push_str(lines[index]);
        patched.push('\n');
        index += 1;
    }

    patched
}

fn parse_pointer_static_decl(line: &str) -> Option<(&str, bool, &str)> {
    let mut decl = line.trim().strip_prefix("pub static ")?;
    let is_mut = decl.starts_with("mut ");
    if is_mut {
        decl = decl.strip_prefix("mut ")?;
    }

    let (name, ty) = decl.split_once(':')?;
    let ty = ty.trim().strip_suffix(';')?;
    if !ty.starts_with('*') {
        return None;
    }

    Some((name.trim(), is_mut, ty))
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
