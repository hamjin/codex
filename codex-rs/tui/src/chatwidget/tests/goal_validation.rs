use super::*;
use pretty_assertions::assert_eq;

const LARGE_PASTE_CHARS: usize = 1_001;

fn submit_composer_text(chat: &mut ChatWidget, text: &str) {
    chat.bottom_pane
        .set_composer_text(text.to_string(), Vec::new(), Vec::new());
    submit_current_composer(chat);
}

fn submit_current_composer(chat: &mut ChatWidget) {
    chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
}

#[tokio::test]
async fn goal_slash_command_accepts_multiline_objective_after_blank_first_line() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::Goals, /*enabled*/ true);
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    let objective = "follow these instructions\npreserve this detail";

    submit_composer_text(&mut chat, &format!("/goal \n\n{objective}"));

    let draft = next_goal_draft(&mut rx, thread_id);
    assert_eq!(draft.objective, objective);
    assert_no_submit_op(&mut op_rx);
}

#[tokio::test]
async fn goal_slash_command_emits_only_inserted_paste_text_element() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_feature_enabled(Feature::Goals, /*enabled*/ true);
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    let paste = "x".repeat(LARGE_PASTE_CHARS);
    let placeholder = format!("[Pasted Content {} chars]", paste.chars().count());
    chat.bottom_pane.set_composer_text(
        format!("/goal keep literal {placeholder} and "),
        Vec::new(),
        Vec::new(),
    );
    chat.handle_paste(paste.clone());

    submit_current_composer(&mut chat);

    let draft = next_goal_draft(&mut rx, thread_id);
    assert!(
        draft
            .objective
            .contains(&format!("keep literal {placeholder} and {placeholder}")),
        "expected literal placeholder and inserted paste placeholder, got {:?}",
        draft.objective
    );
    assert_eq!(draft.pending_pastes, vec![(placeholder, paste)]);
    assert_no_submit_op(&mut op_rx);
}
