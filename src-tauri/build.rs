fn main() {
    tauri_build::build();

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        if let Ok(out_dir) = std::env::var("OUT_DIR") {
            let res = std::path::Path::new(&out_dir).join("libresource.a");
            if res.exists() {
                println!("cargo:rustc-link-arg={}", res.display());
            }
        }
    }
}

