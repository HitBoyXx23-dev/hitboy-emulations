//! Embeds the app icon and product name in the Windows executable.
fn main() {
    println!("cargo:rerun-if-changed=../../../assets/icons/hitboy.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../../assets/icons/hitboy.ico");
        res.set("ProductName", "HitBoy’s Emulations");
        res.set("FileDescription", "HitBoy’s Emulations");
        res.set("CompanyName", "HitBoyXx23");
        res.set("LegalCopyright", "© HitBoyXx23");
        res.set("OriginalFilename", "hitboy.exe");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the icon: {e}");
        }
    }
}
