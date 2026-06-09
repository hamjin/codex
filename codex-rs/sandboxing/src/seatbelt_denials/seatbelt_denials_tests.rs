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

#[test]
fn collected_logs_are_capped_at_one_thousand_characters() {
    let mut log_lines = VecDeque::new();
    let mut collected_chars = 0;
    let old_line = format!("{}\n", "a".repeat(599));
    let recent_line = format!("{}\n", "é".repeat(499));

    append_log_line(&mut log_lines, &mut collected_chars, old_line.as_bytes());
    append_log_line(&mut log_lines, &mut collected_chars, recent_line.as_bytes());

    assert_eq!(log_lines, VecDeque::from([recent_line]));
    assert_eq!(collected_chars, 500);
}
