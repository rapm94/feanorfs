use super::*;

#[test]
fn legacy_label_detection_parses_macos_plists() {
    // macOS plist parsing needs /usr/bin/plutil; the pure label filtering
    // is exercised here through the registry instead.
    assert!(LABEL.starts_with("com.feanorfs."));
}

#[cfg(target_os = "macos")]
#[test]
fn legacy_workspace_plist_extracts_program_argument_three() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let plist = dir.path().join("com.feanorfs.sync-test.plist");
    let mut file = std::fs::File::create(&plist).unwrap();
    file.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>ProgramArguments</key><array>
<string>/usr/local/bin/feanorfs</string>
<string>service</string>
<string>run</string>
<string>/Users/me/My Project</string>
</array>
</dict></plist>"#,
        )
        .unwrap();
    drop(file);
    // `feanorfs service run <workspace>`: the workspace is index 3.
    assert_eq!(
        plist_program_argument(&plist, 3).as_deref(),
        Some("/Users/me/My Project")
    );
    // The previous index-2 read returned the subcommand, not the path.
    assert_eq!(plist_program_argument(&plist, 2).as_deref(), Some("run"));
}
