fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // application manifest (DPI awareness, Win8+ layered child windows, common controls v6)
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}/app.manifest", dir);
    println!("cargo:rerun-if-changed=app.manifest");
    // application icon
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=app.rc");
    embed_resource::compile("app.rc", embed_resource::NONE).manifest_optional().unwrap();
}
