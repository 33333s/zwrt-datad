use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-env-changed=DATAD_KEYMASTER_WORKER");
    println!("cargo:rerun-if-changed=keymaster_worker.rs");
    let worker = if env::var_os("CARGO_FEATURE_KEYMASTER_WORKER").is_some() {
        Vec::new()
    } else {
        match env::var_os("DATAD_KEYMASTER_WORKER") {
            Some(path) => {
                println!("cargo:rerun-if-changed={}", PathBuf::from(&path).display());
                let bytes = fs::read(path).expect("read Keymaster worker");
                assert!(bytes.len() > 64 && bytes.len() < 2 * 1024 * 1024);
                assert_eq!(&bytes[..6], b"\x7fELF\x02\x01");
                assert_eq!(&bytes[18..20], &[183, 0], "worker must be AArch64");
                bytes
            }
            None => {
                assert!(
                    !(env::var("TARGET").unwrap() == "aarch64-unknown-linux-musl"
                        && env::var("PROFILE").unwrap() == "release"),
                    "Build ARM64 releases with scripts/build.sh so the Keymaster worker is embedded"
                );
                Vec::new()
            }
        }
    };
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("keymaster-worker"),
        worker,
    )
    .unwrap();
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
