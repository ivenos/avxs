fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FFMS2_LIB_DIR");

    if let Ok(dir) = std::env::var("FFMS2_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    } else {
        #[cfg(target_os = "linux")]
        println!("cargo:rustc-link-search=native=/usr/local/lib");
    }

    println!("cargo:rustc-link-lib=dylib=ffms2");
}
