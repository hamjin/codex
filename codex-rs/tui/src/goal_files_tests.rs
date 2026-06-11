use super::*;

use codex_protocol::protocol::MAX_THREAD_GOAL_OBJECTIVE_CHARS;
use codex_protocol::user_input::TextElement;
use pretty_assertions::assert_eq;

#[derive(Default)]
struct RecordingStore {
    writes: Vec<(String, Vec<u8>)>,
}

impl GoalFileStore for RecordingStore {
    async fn create_directory(&mut self, _path: GoalFilePath) -> Result<()> {
        Ok(())
    }

    async fn write_file(&mut self, path: GoalFilePath, bytes: Vec<u8>) -> Result<()> {
        self.writes.push((path.as_str().to_string(), bytes));
        Ok(())
    }

    async fn read_file(&mut self, path: GoalFilePath) -> Result<Vec<u8>> {
        self.writes
            .iter()
            .find(|(write_path, _)| write_path == path.as_str())
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| anyhow::anyhow!("missing recording for {path}"))
    }
}

#[tokio::test]
async fn materializes_active_paste_placeholder() {
    let placeholder = "[Pasted Content 5 chars]";
    let objective = format!(
        "Use {placeholder}. {}",
        "x".repeat(MAX_THREAD_GOAL_OBJECTIVE_CHARS + 1)
    );
    let codex_home =
        codex_app_server_client::AppServerPath::from_app_server(r"C:\Users\codex\.codex");
    let mut store = RecordingStore::default();

    let reference = materialize_goal_draft(
        &mut store,
        Some(&codex_home),
        GoalDraft {
            objective: objective.clone(),
            text_elements: vec![TextElement::new(
                (4..4 + placeholder.len()).into(),
                Some(placeholder.to_string()),
            )],
            pending_pastes: vec![(placeholder.to_string(), "hello".to_string())],
        },
    )
    .await
    .expect("materialize goal draft")
    .objective;

    let edit_text = objective_text_for_edit(&mut store, Some(&codex_home), &reference)
        .await
        .expect("read objective text");
    assert!(edit_text.contains(r"pasted text file: C:\Users\codex\.codex\attachments\"));
    assert!(edit_text.contains("Read this file before continuing."));
    assert!(store.writes.iter().any(|(_, bytes)| bytes == b"hello"));
}

#[tokio::test]
async fn whitespace_paste_only_objective_is_empty() {
    let placeholder = "[Pasted Content 3 chars]";
    let mut store = RecordingStore::default();
    let codex_home = codex_app_server_client::AppServerPath::from_app_server("/tmp/codex");

    let err = materialize_goal_draft(
        &mut store,
        Some(&codex_home),
        GoalDraft {
            objective: placeholder.to_string(),
            text_elements: vec![TextElement::new(
                (0..placeholder.len()).into(),
                Some(placeholder.to_string()),
            )],
            pending_pastes: vec![(placeholder.to_string(), " \n\t".to_string())],
        },
    )
    .await
    .expect_err("whitespace-only paste should be rejected");

    assert_eq!(err.to_string(), "Goal objective must not be empty.");
    assert!(store.writes.is_empty());
}

#[tokio::test]
async fn deleted_paste_placeholder_does_not_materialize_or_need_codex_home() {
    let mut store = RecordingStore::default();

    let objective = materialize_goal_draft(
        &mut store,
        /*codex_home*/ None,
        GoalDraft {
            objective: "small goal".to_string(),
            pending_pastes: vec![("[Pasted Content 5 chars]".to_string(), "hello".to_string())],
            ..Default::default()
        },
    )
    .await
    .expect("materialize plain goal draft")
    .objective;

    assert_eq!(objective, "small goal");
    assert!(store.writes.is_empty());
}
