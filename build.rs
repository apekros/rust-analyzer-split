//! Records the triple this is built for: the native rust-analyzer analyzes
//! for the same machine, so it is what "native" means when classifying.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=RUST_ANALYZER_SPLIT_HOST={target}");
}
