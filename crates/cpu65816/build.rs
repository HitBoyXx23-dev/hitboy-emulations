fn main() {
    for f in ["cpp/w65816.cpp", "cpp/w65816.h", "cpp/spc700.cpp", "cpp/spc700.h"] {
        println!("cargo:rerun-if-changed={f}");
    }
    build().cpp(true).file("cpp/w65816.cpp").file("cpp/spc700.cpp").std("c++17").opt_level(3).compile("snescpus");
}

/// A compiler configured for the target; for the browser (WebAssembly) it adds our small
/// C library headers and turns off C++ exceptions and RTTI.
fn build() -> cc::Build {
    let mut b = cc::Build::new();
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        b.cpp_link_stdlib(None).include("../wasmlibc/shim").flag("-fno-exceptions").flag("-fno-rtti");
        println!("cargo:rerun-if-changed=../wasmlibc/shim");
    }
    b
}
