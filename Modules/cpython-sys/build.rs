use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let curdir = std::env::current_dir().unwrap();
    let srcdir = curdir.parent().and_then(Path::parent).unwrap();
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    generate_c_api_bindings(srcdir, &out_path.as_path());
    // TODO(emmatyping): generate bindings to the internal parser API
    // The parser includes things slightly differently, so we should generate
    // it's bindings independently
    //generate_parser_bindings(srcdir, &out_path.as_path());
}

fn generate_c_api_bindings(srcdir: &Path, out_path: &Path) {
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", srcdir.as_os_str().to_str().unwrap()))
        .clang_arg(format!("-I{}/Include", srcdir.as_os_str().to_str().unwrap()))
        .allowlist_function("_?Py.*")
        .allowlist_type("_?Py.*")
        .allowlist_var("_?Py.*")
        .blocklist_type("^PyMethodDef$")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/c_api.rs file.
    bindings
        .write_to_file(out_path.join("c_api.rs"))
        .expect("Couldn't write bindings!");
}
