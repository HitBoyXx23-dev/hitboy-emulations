fn main() {
    println!("cargo:rerun-if-changed=cpp/gekko.cpp");
    println!("cargo:rerun-if-changed=cpp/gekko.h");
    cc::Build::new().cpp(true).file("cpp/gekko.cpp").std("c++17").opt_level(3).compile("gekko");
}
