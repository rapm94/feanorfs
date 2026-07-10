use crate::paths::validate_name;

#[test]
fn validate_name_accepts_simple_identifier() {
    assert!(validate_name("ci1").is_ok());
    assert!(validate_name("agent-foo").is_ok());
    assert!(validate_name("agent_foo").is_ok());
    assert!(validate_name("agent.foo").is_ok());
}

#[test]
fn validate_name_rejects_empty() {
    let error = validate_name("").expect_err("empty name should fail");
    assert!(error.to_string().contains("empty"));
}

#[test]
fn validate_name_rejects_forward_slash() {
    assert!(validate_name("a/b").is_err());
}

#[test]
fn validate_name_rejects_backslash() {
    assert!(validate_name(r"a\b").is_err());
}

#[test]
fn validate_name_rejects_dot() {
    assert!(validate_name(".").is_err());
}

#[test]
fn validate_name_rejects_dotdot() {
    assert!(validate_name("..").is_err());
}

#[test]
fn validate_name_rejects_control_chars() {
    assert!(validate_name("a\nb").is_err());
    assert!(validate_name("a\tb").is_err());
    assert!(validate_name("a\0b").is_err());
}
