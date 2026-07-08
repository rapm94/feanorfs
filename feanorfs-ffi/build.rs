use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let header_path = crate_dir.join("feanorfs.h");
    let config = cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")).unwrap();
    let generated = cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
        .expect("cbindgen");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staged = out_dir.join("feanorfs.h");
    generated.write_to_file(&staged);

    let new_content = fs::read_to_string(&staged).unwrap();
    let old_content = fs::read_to_string(&header_path).unwrap_or_default();
    if new_content != old_content {
        fs::write(&header_path, new_content).unwrap();
    }

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
