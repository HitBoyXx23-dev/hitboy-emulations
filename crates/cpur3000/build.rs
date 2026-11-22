fn main() {
    println!("cargo:rerun-if-changed=cpp/r3000.cpp");
    println!("cargo:rerun-if-changed=cpp/r3000.h");
    cc::Build::new().cpp(true).file("cpp/r3000.cpp").std("c++17").opt_level(3).compile("r3000");
}

