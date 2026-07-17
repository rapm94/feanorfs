use crate::local::validate_e2ee_key;

#[test]
fn generated_key_shape_is_accepted_for_encrypted_formats() {
    let key = feanorfs_common::generate_password().expect("generate key");
    validate_e2ee_key(&key, 2).expect("format v2 key");
    validate_e2ee_key(&key, 3).expect("format v3 key");
}

#[test]
fn encrypted_formats_reject_human_or_noncanonical_keys() {
    for key in [
        "correct horse battery staple".to_string(),
        "A".repeat(64),
        "g".repeat(64),
        "0".repeat(63),
        "0".repeat(65),
    ] {
        let error = validate_e2ee_key(&key, 3).expect_err("weak key must be rejected");
        assert!(error.to_string().contains("brute-forced offline"));
    }
}

#[test]
fn legacy_format_keeps_loading_historical_keys() {
    validate_e2ee_key("historical-human-passphrase", 1).expect("legacy key remains readable");
}
