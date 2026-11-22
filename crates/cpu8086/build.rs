fn main() {
    println!("cargo:rerun-if-changed=cpp/x86.cpp");
    println!("cargo:rerun-if-changed=cpp/x86.h");
    cc::Build::new()
        .cpp(true)
        .file("cpp/x86.cpp")
        .std("c++17")
        .opt_level(3)
        .compile("cpu8086");
}
