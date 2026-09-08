use std::path::PathBuf;

fn main() {
    // Test binaries reference Python C API symbols through the crate's glue
    // (trampolines, statics). Link test executables the way an extension
    // module is linked — Python symbols may stay unresolved — so `cargo
    // test` still compiles in a tree where libpython has not been built yet
    // (as in CI, which runs configure + cargo test without make). When the
    // build dir does contain a static libpython, link it so the symbols the
    // test binary touches at load time resolve for real and the tests can
    // run.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        return;
    }
    println!("cargo:rustc-link-arg-tests=-Wl,--unresolved-symbols=ignore-all");
    // Rust links with `-z now` by default, which would make the dynamic
    // loader abort on any interpreter symbol the link left unresolved even
    // though the tests never call one. Lazy binding defers function
    // resolution to first call.
    println!("cargo:rustc-link-arg-tests=-Wl,-z,lazy");

    println!("cargo:rerun-if-env-changed=PYTHON_BUILD_DIR");
    let Ok(build_dir) = std::env::var("PYTHON_BUILD_DIR") else {
        return;
    };
    let build_dir = PathBuf::from(build_dir);
    println!(
        "cargo:rerun-if-changed={}",
        build_dir.join("libpython3.15.a").display()
    );
    let mut libs: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&build_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_prefix("lib").and_then(|n| n.strip_suffix(".a"))
                && stem.starts_with("python")
            {
                libs.push(stem.to_owned());
            }
        }
    }
    // Highest version wins if several are lying around.
    libs.sort();
    println!("cargo:rustc-check-cfg=cfg(has_libpython)");
    if let Some(lib) = libs.last() {
        println!("cargo:rustc-link-arg-tests=-L{}", build_dir.display());
        println!("cargo:rustc-link-arg-tests=-l{lib}");
        for sys in ["m", "dl", "pthread", "util"] {
            println!("cargo:rustc-link-arg-tests=-l{sys}");
        }
        // Tests that need real interpreter symbols at load time are gated on
        // this cfg, so `cargo test` stays green in a tree where CPython
        // itself hasn't been built.
        println!("cargo:rustc-cfg=has_libpython");
    }
}
