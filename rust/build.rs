use std::{env, fs, path::PathBuf};
fn main() {
    let path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../version.json");
    println!("cargo:rerun-if-changed={}", path.display());
    let text = fs::read_to_string(path).expect("read version.json");
    let marker = "\"version\":";
    let rest = text.split(marker).nth(1).expect("version field");
    let version = rest.split('"').nth(1).expect("version value");
    assert!(
        version.split('.').count() == 3 && version.chars().all(|c| c.is_ascii_digit() || c == '.')
    );
    println!("cargo:rustc-env=DATAD_VERSION={version}");
}
