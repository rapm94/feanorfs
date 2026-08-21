feanorfs_test_support::isolate_test_process!();

// F2 symbol parity: every `#[no_mangle] pub [unsafe] extern "C"` export in
// `src/lib.rs` must be declared in the regenerated `feanorfs.h`, and the
// header must not declare any symbol the crate does not export. The header is
// regenerated through the crate's own build (build.rs runs cbindgen), exactly
// as the acceptance flow does.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Nested cargo builds must never race each other on the target-dir lock.
static CARGO_BUILD_GUARD: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Regenerate `feanorfs.h` via `cargo build -p feanorfs-ffi`, then return it.
fn regenerated_header() -> String {
    let _guard = CARGO_BUILD_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", "feanorfs-ffi", "--locked"])
        .env("CARGO_TERM_COLOR", "never")
        .current_dir(crate_dir())
        .output()
        .expect("failed to spawn cargo build for header regeneration");
    assert!(
        output.status.success(),
        "cargo build -p feanorfs-ffi --locked failed while regenerating feanorfs.h:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(crate_dir().join("feanorfs.h"))
        .expect("feanorfs.h unreadable after regeneration")
}

/// Enumerate `#[no_mangle] pub [unsafe] extern "C" fn NAME` exports.
fn rust_exports(lib_rs: &str) -> BTreeSet<String> {
    let mut exports = BTreeSet::new();
    let mut lines = lib_rs.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "#[no_mangle]" {
            continue;
        }
        // The `fn NAME` token always sits on the line after the attribute,
        // even when the parameter list continues on later lines.
        let signature = lines
            .next()
            .unwrap_or_else(|| panic!("`#[no_mangle]` with no following signature"));
        let name = signature
            .split("fn ")
            .nth(1)
            .and_then(|rest| rest.split('(').next())
            .and_then(|token| token.split_whitespace().next())
            .unwrap_or_else(|| panic!("could not parse exported fn name from: {signature:?}"));
        assert!(
            name.starts_with("ffs_"),
            "unexpected export name {name:?} (expected an `ffs_` symbol)"
        );
        exports.insert(name.to_string());
    }
    exports
}

/// Enumerate function declarations in the generated header.
fn header_declarations(header: &str) -> BTreeSet<String> {
    let mut declarations = BTreeSet::new();
    for line in header.lines() {
        let trimmed = line.trim_start();
        // Only real declarations: `const char *ffs_x(`, `int32_t ffs_x(`,
        // `void ffs_x(` at column zero. Comments never match.
        let rest = trimmed.strip_prefix("const char *").or_else(|| {
            trimmed
                .strip_prefix("int32_t ")
                .or_else(|| trimmed.strip_prefix("void "))
        });
        let Some(rest) = rest else { continue };
        let name = rest.split('(').next().unwrap_or_default().trim();
        if name.starts_with("ffs_") {
            declarations.insert(name.to_string());
        }
    }
    declarations
}

#[test]
fn every_rust_export_is_declared_in_the_regenerated_header() {
    let lib_rs =
        std::fs::read_to_string(crate_dir().join("src/lib.rs")).expect("src/lib.rs unreadable");
    let header = regenerated_header();
    let exports = rust_exports(&lib_rs);
    let declarations = header_declarations(&header);

    assert!(
        !exports.is_empty(),
        "no `#[no_mangle]` exports were enumerated"
    );
    assert!(
        !declarations.is_empty(),
        "no `ffs_*` declarations were found in the regenerated header"
    );

    for export in &exports {
        assert!(
            declarations.contains(export),
            "Rust export `{export}` is missing from the regenerated feanorfs.h"
        );
    }
    for declared in &declarations {
        assert!(
            exports.contains(declared),
            "feanorfs.h declares `{declared}`, which has no `#[no_mangle]` export in src/lib.rs"
        );
    }
    // Exact enumeration: neither side may drift independently.
    assert_eq!(
        exports, declarations,
        "C header and Rust exports must enumerate exactly the same symbols"
    );
}

#[test]
fn regenerated_header_compiles_as_c() {
    // Header compile smoke (F2 gate). Skipped when no C compiler is present.
    let header = regenerated_header();
    let output = Command::new("cc")
        .args(["-fsyntax-only", "-xc", "-", "-Wall", "-Werror"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match output {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping C compile smoke: no `cc` on PATH");
            return;
        }
        Err(error) => panic!("could not spawn `cc`: {error}"),
    };
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().expect("cc stdin");
        stdin
            .write_all(header.as_bytes())
            .expect("write header to cc");
        stdin.flush().expect("flush cc stdin");
    }
    let result = child.wait_with_output().expect("wait for cc");
    assert!(
        result.status.success(),
        "regenerated feanorfs.h does not compile as C:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
