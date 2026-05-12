fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // libffms2.so is installed to /usr/local/lib in the Docker image
    println!("cargo:rustc-link-search=native=/usr/local/lib");
    println!("cargo:rustc-link-lib=dylib=ffms2");
}
