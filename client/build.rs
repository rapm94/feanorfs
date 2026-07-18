fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("-pc-windows-msvc") {
        // The Windows PE default is 1 MiB, which is too small for the Tokio
        // entrypoint plus the complete onboarding/sync state machine.
        println!("cargo:rustc-link-arg-bin=feanorfs=/STACK:8388608");
    }
}
