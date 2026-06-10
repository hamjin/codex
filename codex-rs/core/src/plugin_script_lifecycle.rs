use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

use codex_analytics::AnalyticsEventsClient;
use codex_analytics::CodexPluginScriptLifecycleEvent;
use codex_analytics::PluginScriptLifecycleStatus;
use codex_analytics::PluginScriptSkill;
use codex_features::Feature;
use codex_plugin::FirstPartyPluginRoot;
use codex_utils_absolute_path::AbsolutePathBuf;
use uuid::Uuid;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::shell::ShellType;
use crate::skills::SkillLoadOutcome;

#[derive(Debug)]
struct ResolvedPluginScript {
    plugin_id: String,
    script_path: String,
    skill: Option<PluginScriptSkill>,
}

/// Tracks one actual plugin-script process execution.
///
/// Resolution happens before process launch, but the first event is not emitted
/// until the process spawn callback runs. Terminal calls are idempotent because
/// unified exec can observe the same process through several paths.
pub(crate) struct PluginScriptExecution {
    analytics: AnalyticsEventsClient,
    event: CodexPluginScriptLifecycleEvent,
    started_at: OnceLock<Instant>,
    terminal_emitted: AtomicBool,
    cancelled: AtomicBool,
}

impl PluginScriptExecution {
    pub(crate) fn resolve(
        session: &Session,
        turn: &TurnContext,
        command: &str,
        cwd: &AbsolutePathBuf,
        shell_type: ShellType,
    ) -> Option<Arc<Self>> {
        if !turn
            .features
            .enabled(Feature::PluginScriptLifecycleAnalytics)
        {
            return None;
        }

        let resolved = resolve_plugin_script(
            &turn.first_party_plugin_roots,
            &turn.turn_skills.outcome,
            command,
            cwd,
            shell_type,
        )?;

        Some(Arc::new(Self {
            analytics: session.services.analytics_events_client.clone(),
            event: CodexPluginScriptLifecycleEvent {
                thread_id: session.thread_id.to_string(),
                turn_id: turn.sub_id.clone(),
                plugin_id: resolved.plugin_id,
                execution_id: Uuid::new_v4().to_string(),
                script_path: resolved.script_path,
                status: PluginScriptLifecycleStatus::Started,
                duration_ms: None,
                exit_code: None,
                skill: resolved.skill,
            },
            started_at: OnceLock::new(),
            terminal_emitted: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }))
    }

    pub(crate) fn mark_started(&self) {
        if self.started_at.set(Instant::now()).is_err() {
            return;
        }
        self.analytics
            .track_plugin_script_lifecycle(self.event.clone());
    }

    pub(crate) fn mark_cancelled(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn finish(&self, exit_code: Option<i32>, failed: bool) {
        let Some(started_at) = self.started_at.get() else {
            return;
        };
        if self.terminal_emitted.swap(true, Ordering::AcqRel) {
            return;
        }

        let status = if self.cancelled.load(Ordering::Acquire) {
            PluginScriptLifecycleStatus::Cancelled
        } else if !failed && exit_code == Some(0) {
            PluginScriptLifecycleStatus::Completed
        } else {
            PluginScriptLifecycleStatus::Failed
        };
        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.analytics
            .track_plugin_script_lifecycle(CodexPluginScriptLifecycleEvent {
                status,
                duration_ms: Some(duration_ms),
                exit_code,
                ..self.event.clone()
            });
    }
}

fn resolve_plugin_script(
    plugin_roots: &[FirstPartyPluginRoot],
    skills_outcome: &SkillLoadOutcome,
    command: &str,
    cwd: &AbsolutePathBuf,
    shell_type: ShellType,
) -> Option<ResolvedPluginScript> {
    let script_token = script_token(command, shell_type)?;
    let script_path = Path::new(&script_token);
    let script_path = if script_path.is_absolute() {
        AbsolutePathBuf::try_from(script_path).ok()?
    } else {
        cwd.join(script_path)
    };
    let script_path = script_path.canonicalize().ok()?;

    let (root, plugin_root) = plugin_roots
        .iter()
        .filter_map(|root| {
            let plugin_root = root.plugin_root.canonicalize().ok()?;
            script_path.strip_prefix(&plugin_root).ok()?;
            Some((root, plugin_root))
        })
        .max_by_key(|(_, plugin_root)| plugin_root.components().count())?;
    let relative = script_path.strip_prefix(plugin_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(ResolvedPluginScript {
        plugin_id: root.plugin_id.clone(),
        script_path: normalized_relative_path(relative),
        skill: skill_for_script(skills_outcome, &root.plugin_id, &script_path),
    })
}

fn skill_for_script(
    skills_outcome: &SkillLoadOutcome,
    plugin_id: &str,
    script_path: &Path,
) -> Option<PluginScriptSkill> {
    skills_outcome.skills.iter().find_map(|skill| {
        if skill.plugin_id.as_deref() != Some(plugin_id) || !skills_outcome.is_skill_enabled(skill)
        {
            return None;
        }
        let scripts_dir = skill.path_to_skills_md.parent()?.join("scripts");
        let scripts_dir = scripts_dir.canonicalize().ok()?;
        script_path.strip_prefix(scripts_dir).ok()?;
        Some(PluginScriptSkill {
            skill_name: skill.name.clone(),
            skill_path: skill.path_to_skills_md.clone().into_path_buf(),
        })
    })
}

fn script_token(command: &str, shell_type: ShellType) -> Option<String> {
    let tokens = command_tokens(command, shell_type)?;
    let program = tokens.first()?;
    let basename = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())?
        .to_string();
    let windows_shell = matches!(shell_type, ShellType::PowerShell | ShellType::Cmd);
    let basename = if windows_shell {
        basename.to_ascii_lowercase()
    } else {
        basename
    };
    let basename = if windows_shell {
        basename.strip_suffix(".exe").unwrap_or(&basename)
    } else {
        &basename
    };
    let args = &tokens[1..];
    let runner_script = match basename {
        "python" | "python3" | "node" | "bash" | "zsh" | "sh" => {
            args.first().filter(|arg| !arg.starts_with('-')).cloned()
        }
        "pwsh" | "powershell" => match args {
            [option, script, ..]
                if matches!(option.to_ascii_lowercase().as_str(), "-file" | "-f") =>
            {
                Some(script.clone())
            }
            _ => None,
        },
        _ => None,
    };
    if runner_script.is_some() {
        return runner_script;
    }
    if matches!(
        basename,
        "python" | "python3" | "bash" | "zsh" | "sh" | "node" | "pwsh" | "powershell"
    ) {
        return None;
    }

    let path = Path::new(program);
    (path.is_absolute() || program.contains('/') || program.contains('\\')).then(|| program.clone())
}

fn command_tokens(command: &str, shell_type: ShellType) -> Option<Vec<String>> {
    match shell_type {
        ShellType::Bash | ShellType::Sh | ShellType::Zsh => {
            let tree = codex_shell_command::bash::try_parse_shell(command)?;
            let mut commands =
                codex_shell_command::bash::try_parse_word_only_commands_sequence(&tree, command)?;
            let [tokens] = commands.as_mut_slice() else {
                return None;
            };
            Some(std::mem::take(tokens))
        }
        ShellType::PowerShell => split_windows_command(command),
        ShellType::Cmd => None,
    }
}

/// Splits one plain PowerShell-style command without treating backslashes as
/// escapes. Compound commands are rejected because lifecycle events attach to
/// the spawned shell process and cannot represent multiple child scripts.
fn split_windows_command(command: &str) -> Option<Vec<String>> {
    let mut chars = command.chars().peekable();
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut saw_token = false;

    while let Some(ch) = chars.next() {
        if let Some(active_quote) = quote {
            if ch == '`' {
                token.push(chars.next()?);
            } else if ch == active_quote {
                if chars.peek() == Some(&active_quote) {
                    token.push(active_quote);
                    chars.next();
                } else {
                    quote = None;
                }
            } else {
                token.push(ch);
            }
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                saw_token = true;
            }
            '`' => {
                token.push(chars.next()?);
                saw_token = true;
            }
            ' ' | '\t' => {
                if saw_token {
                    tokens.push(std::mem::take(&mut token));
                    saw_token = false;
                }
            }
            '&' if tokens.is_empty() && !saw_token => {
                if !chars.peek().is_some_and(|next| next.is_whitespace()) {
                    return None;
                }
            }
            '&' | '|' | ';' | '\r' | '\n' => return None,
            _ => {
                token.push(ch);
                saw_token = true;
            }
        }
    }

    if quote.is_some() {
        return None;
    }
    if saw_token {
        tokens.push(token);
    }
    (!tokens.is_empty()).then_some(tokens)
}

fn normalized_relative_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
#[path = "plugin_script_lifecycle_tests.rs"]
mod tests;
