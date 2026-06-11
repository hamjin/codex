//! File materialization helpers for TUI goal objectives.
//!
//! Long objectives and pasted text are written under the app server's Codex
//! home directory. The persisted goal objective keeps file references so later
//! continuations can read the long inputs by path.

use crate::app_server_session::AppServerSession;
use crate::bottom_pane::ChatComposer;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_app_server_client::AppServerPath;
use codex_protocol::protocol::MAX_THREAD_GOAL_OBJECTIVE_CHARS;
use codex_protocol::user_input::TextElement;
use uuid::Uuid;

const GOAL_ATTACHMENT_DIR: &str = "attachments";
const GOAL_FILE_PREFIX: &str = "Codex goal objective file: ";
const GOAL_FILE_INSTRUCTION: &str = "Read that Codex-created file before continuing.";
const GOAL_FILE_NAME: &str = "goal-objective.md";

#[derive(Clone, Debug, Default)]
pub(crate) struct GoalDraft {
    pub(crate) objective: String,
    pub(crate) text_elements: Vec<TextElement>,
    pub(crate) pending_pastes: Vec<(String, String)>,
}

/// Host-side file operations needed to materialize goal inputs.
///
/// Implementations must operate on the same filesystem that the app server and
/// agent will use to resolve persisted goal file references.
pub(crate) trait GoalFileStore {
    fn create_directory(
        &mut self,
        path: GoalFilePath,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    fn write_file(
        &mut self,
        path: GoalFilePath,
        bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    fn read_file(
        &mut self,
        path: GoalFilePath,
    ) -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;
}

pub(crate) type GoalFilePath = AppServerPath;

impl GoalFileStore for AppServerSession {
    async fn create_directory(&mut self, path: GoalFilePath) -> Result<()> {
        self.fs_create_directory_all_path(&path)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    async fn write_file(&mut self, path: GoalFilePath, bytes: Vec<u8>) -> Result<()> {
        self.fs_write_file_path(&path, bytes)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    async fn read_file(&mut self, path: GoalFilePath) -> Result<Vec<u8>> {
        self.fs_read_file_path(&path)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
    }
}

pub(crate) async fn materialize_goal_draft(
    store: &mut impl GoalFileStore,
    codex_home: Option<&GoalFilePath>,
    draft: GoalDraft,
) -> Result<String> {
    let mut objective = draft.objective;
    if objective.trim().is_empty() {
        bail!("Goal objective must not be empty.");
    }
    let text_elements = draft.text_elements;
    if !draft.pending_pastes.is_empty() {
        let (expanded_objective, _) = ChatComposer::expand_pending_pastes(
            &objective,
            text_elements.clone(),
            &draft.pending_pastes,
        );
        if expanded_objective.trim().is_empty() {
            bail!("Goal objective must not be empty.");
        }
    }

    let mut active_placeholders = text_elements
        .iter()
        .filter_map(|element| element.placeholder(&objective))
        .filter(|placeholder| !placeholder.is_empty())
        .collect::<Vec<_>>();
    let mut output_dir = None;
    let mut replacements = Vec::new();
    for (placeholder, text) in draft.pending_pastes.iter() {
        let Some(active_idx) = active_placeholders
            .iter()
            .position(|active| *active == placeholder.as_str())
        else {
            continue;
        };
        active_placeholders.swap_remove(active_idx);
        let path = ensure_goal_output_dir(store, codex_home, &mut output_dir)
            .await?
            .join(format!("pasted-text-{}.txt", replacements.len() + 1));
        write_goal_file(store, path.clone(), text.as_bytes().to_vec()).await?;

        replacements.push((
            placeholder.clone(),
            format!("pasted text file: {path}. Read this file before continuing."),
        ));
    }

    let (expanded_objective, _) =
        ChatComposer::expand_pending_pastes(&objective, text_elements, &replacements);
    objective = expanded_objective.trim().to_string();

    if objective.chars().count() > MAX_THREAD_GOAL_OBJECTIVE_CHARS {
        let path = ensure_goal_output_dir(store, codex_home, &mut output_dir)
            .await?
            .join(GOAL_FILE_NAME);
        write_goal_file(store, path.clone(), objective.as_bytes().to_vec()).await?;
        objective = objective_file_reference(&path)?;
    }

    Ok(objective)
}

pub(crate) async fn objective_text_for_edit(
    store: &mut impl GoalFileStore,
    codex_home: Option<&GoalFilePath>,
    objective: &str,
) -> Result<String> {
    let Some(path) = objective_file_path(objective, codex_home) else {
        return Ok(objective.to_string());
    };
    let bytes = store
        .read_file(path.clone())
        .await
        .with_context(|| format!("Could not read goal objective file {path}"))?;
    String::from_utf8(bytes)
        .with_context(|| format!("Goal objective file {path} is not valid UTF-8"))
}

pub(crate) fn objective_file_path(
    objective: &str,
    codex_home: Option<&GoalFilePath>,
) -> Option<GoalFilePath> {
    let path = parse_objective_file_path(objective)?;
    let codex_home = codex_home?;
    let codex_home_parts = codex_home.components();
    let path_parts = path.components();
    (!codex_home_parts.is_empty()
        && !has_normalization_component(&codex_home_parts)
        && !has_normalization_component(&path_parts)
        && path_parts.starts_with(&codex_home_parts))
    .then_some(path)
}

fn has_normalization_component(parts: &[&str]) -> bool {
    parts.iter().any(|part| matches!(*part, "." | ".."))
}

fn parse_objective_file_path(objective: &str) -> Option<GoalFilePath> {
    let mut lines = objective.lines();
    let path = lines
        .next()?
        .strip_prefix(GOAL_FILE_PREFIX)
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    if lines.next() != Some(GOAL_FILE_INSTRUCTION) {
        return None;
    }

    let path = AppServerPath::from_absolute_str(path)?;
    let parts = path.components();
    let file_name = parts.last()?;
    let attachment_id = parts.get(parts.len().checked_sub(2)?)?;
    let attachment_dir = parts.get(parts.len().checked_sub(3)?)?;
    (*file_name == GOAL_FILE_NAME
        && *attachment_dir == GOAL_ATTACHMENT_DIR
        && Uuid::parse_str(attachment_id).is_ok())
    .then_some(path)
}

pub(crate) fn objective_file_reference(path: &GoalFilePath) -> Result<String> {
    let reference = format!("{GOAL_FILE_PREFIX}{path}\n{GOAL_FILE_INSTRUCTION}");
    let actual_chars = reference.chars().count();
    if actual_chars > MAX_THREAD_GOAL_OBJECTIVE_CHARS {
        bail!(
            "Goal objective file reference is too long: {actual_chars} characters. Limit: {MAX_THREAD_GOAL_OBJECTIVE_CHARS} characters."
        );
    }
    Ok(reference)
}

async fn ensure_goal_output_dir(
    store: &mut impl GoalFileStore,
    codex_home: Option<&GoalFilePath>,
    output_dir: &mut Option<GoalFilePath>,
) -> Result<GoalFilePath> {
    if let Some(output_dir) = output_dir {
        return Ok(output_dir.clone());
    }
    let codex_home = codex_home
        .context("App server did not report $CODEX_HOME; cannot materialize goal files")?;
    let path = codex_home
        .join(GOAL_ATTACHMENT_DIR)
        .join(Uuid::new_v4().to_string());
    store
        .create_directory(path.clone())
        .await
        .with_context(|| format!("Could not create goal attachment directory {path}"))?;
    *output_dir = Some(path.clone());
    Ok(path)
}

async fn write_goal_file(
    store: &mut impl GoalFileStore,
    path: GoalFilePath,
    bytes: Vec<u8>,
) -> Result<()> {
    store
        .write_file(path.clone(), bytes)
        .await
        .with_context(|| format!("Could not write goal file {path}"))
}

#[cfg(test)]
#[path = "goal_files_tests.rs"]
mod tests;
