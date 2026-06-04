use super::*;
use pretty_assertions::assert_eq;

#[test]
fn parses_denial_message() {
    assert_eq!(
        parse_message("Sandbox: touch(1234) deny(1) file-write-create /private/tmp/nope"),
        Some((
            1234,
            "touch".to_string(),
            "file-write-create /private/tmp/nope".to_string(),
        ))
    );
}

#[test]
fn formats_denials_for_command_output() {
    let formatted = format_sandbox_denials(&[SandboxDenial {
        name: "touch".to_string(),
        capability: "file-write-create /private/tmp/nope".to_string(),
    }])
    .expect("denial text");

    assert_eq!(
        String::from_utf8_lossy(&formatted),
        "\n=== Sandbox denials ===\n(touch) file-write-create /private/tmp/nope\n"
    );
}
