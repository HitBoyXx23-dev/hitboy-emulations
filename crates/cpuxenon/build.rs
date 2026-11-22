fn main() {
    println!("cargo:rerun-if-changed=cpp/xenon.cpp");
    println!("cargo:rerun-if-changed=cpp/xenon.h");
    cc::Build::new().cpp(true).file("cpp/xenon.cpp").std("c++17").opt_level(3).compile("xenon");
}
