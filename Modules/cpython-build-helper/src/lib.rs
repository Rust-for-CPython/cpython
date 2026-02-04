use std::env;

/// Print necessary link arguments for the library depending on the build
/// configuration (static or shared)
pub fn print_linker_args() {
    let shared_build = env::var("RUST_SHARED_BUILD").expect("RUST_SHARED_BUILD not set in Makefile?");
    if shared_build == "1" {
        let build_shared_args =
            env::var("BLDSHARED_ARGS").expect("BLDSHARED_ARGS not set in Makefile?");
        // TODO(emmatyping): Ideally, we would not need to split the args here and take shlex
        // as a dependency.
        for arg in shlex::split(&build_shared_args).expect("Invalid BUILDSHARED_ARGS") {
            println!("cargo:rustc-link-arg={}", arg);
        }
    }
    // Static linker configuration is in cpython-rust-staticlib
}
