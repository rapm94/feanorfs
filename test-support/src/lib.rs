//! Process-wide isolation for tests that may resolve FeanorFS global state.

use std::sync::Mutex;

static TEST_ROOT: Mutex<Option<tempfile::TempDir>> = Mutex::new(None);

#[ctor::ctor(unsafe)]
fn isolate_test_process() {
    let root = tempfile::Builder::new()
        .prefix("feanorfs-test-")
        .tempdir()
        .expect("create isolated FeanorFS test root");
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("create isolated FeanorFS test home");
    std::env::set_var("HOME", &home);
    std::env::set_var("USERPROFILE", &home);
    std::env::set_var("FEANORFS_HOME", home.join(".feanorfs"));
    std::env::set_var("FEANORFS_CREDENTIAL_STORE", "file");
    *TEST_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root);
}

#[dtor::dtor(unsafe)]
fn clean_test_process() {
    let root = TEST_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(root);
}

/// Link-time anchor used by [`isolate_test_process!`].
#[doc(hidden)]
pub const fn ensure_linked() {}

/// Links the pre-main test-profile isolator into the current test executable.
#[macro_export]
macro_rules! isolate_test_process {
    () => {
        #[used]
        static FEANORFS_TEST_SUPPORT_LINK: fn() = $crate::ensure_linked;
    };
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn constructor_installs_one_private_process_home() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let profile = PathBuf::from(std::env::var_os("FEANORFS_HOME").expect("FEANORFS_HOME"));
        assert_eq!(profile, home.join(".feanorfs"));
        assert!(home.is_dir());
        assert_eq!(
            std::env::var("FEANORFS_CREDENTIAL_STORE").as_deref(),
            Ok("file")
        );
    }

    #[test]
    fn every_state_capable_test_target_links_the_isolator() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repository root");
        let unit_targets = [
            "agent-core/src/lib.rs",
            "client/src/lib.rs",
            "client/src/main.rs",
            "feanorfs-ffi/src/lib.rs",
            "tray/src/main.rs",
        ];
        for relative in unit_targets {
            assert_linked(&repo.join(relative));
        }
        for directory in [
            "agent-core/tests",
            "client/tests",
            "feanorfs-ffi/tests",
            "tray/tests",
        ] {
            let directory = repo.join(directory);
            if !directory.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(directory).expect("read tests directory") {
                let path = entry.expect("test entry").path();
                if path.extension().is_some_and(|extension| extension == "rs") {
                    assert_linked(&path);
                }
            }
        }
    }

    fn assert_linked(path: &std::path::Path) {
        let source = std::fs::read_to_string(path).expect("read test target");
        assert!(
            source.contains("feanorfs_test_support::isolate_test_process!();"),
            "{} does not link process-wide test isolation",
            path.display()
        );
    }
}
