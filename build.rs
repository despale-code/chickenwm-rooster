//! Links the program icon (`assets/icon.ico`, precompiled into `assets/icon-res.o`) into the exe,
//! so Explorer, the Start Menu and the taskbar show it.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon-res.o");
    let windows_x64 = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64");
    if windows_x64 {
        let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        println!("cargo:rustc-link-arg-bins={}", dir.join("assets").join("icon-res.o").display());
    }
}
