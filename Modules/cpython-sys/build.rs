use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let curdir = std::env::current_dir().unwrap();
    let srcdir = curdir.parent().and_then(Path::parent).unwrap();
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", srcdir.as_os_str().to_str().unwrap()))
        .clang_arg(format!("-I{}/Include", srcdir.as_os_str().to_str().unwrap()))
        .clang_arg(format!("-I{}/Include/internal", srcdir.as_os_str().to_str().unwrap()))
        .allowlist_function("Py.*")
        .allowlist_function("_Py.*")
        .allowlist_type("Py.*")
        .allowlist_type("_Py.*")
        .allowlist_var("Py.*")
        .allowlist_var("_Py.*")
        .blocklist_type("^PyMethodDef$")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
