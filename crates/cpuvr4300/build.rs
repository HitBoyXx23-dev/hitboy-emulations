fn main() {
    println!("cargo:rerun-if-changed=cpp/vr4300.cpp");
    println!("cargo:rerun-if-changed=cpp/vr4300.h");
    cc::Build::new().cpp(true).file("cpp/vr4300.cpp").std("c++17").opt_level(3).compile("vr4300");
}
