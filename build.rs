//! Links the program icon (`assets/icon.ico`, precompiled into `assets/icon-res.o`) into the exe,
//! so Explorer, the Start Menu and the taskbar show it.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon-res.o");
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let windows_x64 = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64");
    if windows_x64 {
        let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let src = dir.join("assets").join("icon-res.o");
        if src.exists() {
            let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
            let dest = out_dir.join("icon-res.o");
            let _ = std::fs::copy(&src, &dest);
            println!("cargo:rustc-link-arg-bins={}", dest.display());
        }
    }
}
