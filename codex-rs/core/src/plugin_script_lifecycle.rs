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
) -> Option<ResolvedPluginScript> {
    let script_token = script_token(command)?;
    let script_path = Path::new(&script_token);
    let script_path = if script_path.is_absolute() {
        script_path.to_path_buf()
    } else {
        cwd.join(script_path).into_path_buf()
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

fn script_token(command: &str) -> Option<String> {
    let tokens = command_tokens(command)?;
    let program = tokens.first()?;
    let basename = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())?;
    #[cfg(windows)]
    let basename = basename.to_ascii_lowercase();
    #[cfg(windows)]
    let basename = basename.strip_suffix(".exe").unwrap_or(&basename);
    let args = &tokens[1..];
    let runner_script = match basename {
        "python" | "python3" => script_after_options(
            args,
            &["-W", "-X", "--check-hash-based-pycs"],
            &["-", "-c", "-m", "-h", "--help", "-V", "--version"],
        ),
        "bash" | "zsh" | "sh" => script_after_options(
            args,
            &["-o", "--rcfile", "--init-file"],
            &["-c", "-s", "--help", "--version"],
        ),
        "node" => script_after_options(
            args,
            &[
                "-r",
                "--require",
                "--loader",
                "--experimental-loader",
                "--import",
                "--conditions",
                "--env-file",
                "--input-type",
                "--inspect-port",
                "--title",
            ],
            &[
                "-c",
                "--check",
                "-e",
                "--eval",
                "-p",
                "--print",
                "-h",
                "--help",
                "-v",
                "--version",
            ],
        ),
        "deno" => deno_script(args),
        "ruby" => script_after_options(args, &["-I", "-r"], &["-e", "--eval"]),
        "perl" => script_after_options(args, &["-I", "-M", "-m"], &["-e", "-E"]),
        "pwsh" | "powershell" => powershell_script(args),
        _ => None,
    };
    if runner_script.is_some() {
        return runner_script;
    }
    if matches!(
        basename,
        "python"
            | "python3"
            | "bash"
            | "zsh"
            | "sh"
            | "node"
            | "deno"
            | "ruby"
            | "perl"
            | "pwsh"
            | "powershell"
    ) {
        return None;
    }

    let path = Path::new(program);
    (path.is_absolute() || program.contains('/') || program.contains('\\')).then(|| program.clone())
}

fn script_after_options(
    args: &[String],
    options_with_values: &[&str],
    no_script_options: &[&str],
) -> Option<String> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            return args.get(index + 1).cloned();
        }
        if no_script_options.iter().any(|option| {
            arg == option
                || (option.starts_with("--") && arg.starts_with(&format!("{option}=")))
                || (option.len() == 2 && arg.starts_with(option) && arg.len() > option.len())
        }) {
            return None;
        }
        if options_with_values.contains(&arg.as_str()) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(arg.clone());
    }
    None
}

fn deno_script(args: &[String]) -> Option<String> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let lower = arg.to_ascii_lowercase();
        if matches!(lower.as_str(), "--config" | "--import-map" | "--location") {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if !matches!(lower.as_str(), "run" | "test" | "bench") {
            return None;
        }
        return script_after_options(
            &args[index + 1..],
            &[
                "--config",
                "--import-map",
                "--location",
                "--seed",
                "--v8-flags",
            ],
            &[],
        );
    }
    None
}

fn powershell_script(args: &[String]) -> Option<String> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let lower = arg.to_ascii_lowercase();
        if matches!(lower.as_str(), "-command" | "-c" | "-encodedcommand" | "-e") {
            return None;
        }
        if matches!(lower.as_str(), "-file" | "-f") {
            return args.get(index + 1).cloned();
        }
        if lower == "-workingdirectory" || lower.starts_with("-workingdirectory:") {
            return None;
        }
        if matches!(
            lower.as_str(),
            "-executionpolicy" | "-inputformat" | "-outputformat" | "-windowstyle"
        ) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(arg.clone());
    }
    None
}

#[cfg(not(windows))]
fn command_tokens(command: &str) -> Option<Vec<String>> {
    if has_unquoted_compound_operator(command) {
        return None;
    }
    let mut tokens = shlex::split(command)?;
    while tokens.first().is_some_and(|token| is_env_assignment(token)) {
        tokens.remove(0);
    }
    if tokens.first().is_some_and(|token| {
        Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "env")
    }) {
        tokens.remove(0);
        while let Some(token) = tokens.first() {
            if is_env_assignment(token)
                || matches!(
                    token.as_str(),
                    "-i" | "--ignore-environment" | "-0" | "--null"
                )
            {
                tokens.remove(0);
            } else if matches!(token.as_str(), "-u" | "--unset") {
                if tokens.len() < 2 {
                    return None;
                }
                tokens.drain(..2);
            } else if matches!(token.as_str(), "-C" | "--chdir" | "-S" | "--split-string")
                || token.starts_with("--chdir=")
                || token.starts_with("--split-string=")
                || (token.starts_with("-C") && token.len() > 2)
                || (token.starts_with("-S") && token.len() > 2)
            {
                // These options change how subsequent tokens are parsed or
                // resolved. Skip lifecycle attribution rather than guess.
                return None;
            } else if token == "--" {
                tokens.remove(0);
                break;
            } else if token.starts_with('-') {
                return None;
            } else {
                break;
            }
        }
    }
    (!tokens.is_empty()).then_some(tokens)
}

#[cfg(windows)]
fn command_tokens(command: &str) -> Option<Vec<String>> {
    split_windows_command(command)
}

/// Splits one plain PowerShell-style command without treating backslashes as
/// escapes. Compound commands are rejected because lifecycle events attach to
/// the spawned shell process and cannot represent multiple child scripts.
#[cfg(any(windows, test))]
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

#[cfg(any(not(windows), test))]
fn has_unquoted_compound_operator(command: &str) -> bool {
    let mut chars = command.chars().peekable();
    let mut quote = None;
    let mut escaped = false;
    let mut previous_non_whitespace = None;

    while let Some(ch) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(active_quote) = quote {
            if active_quote == '"' && ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => escaped = true,
            '|' | ';' | '\r' | '\n' => return true,
            '&' if previous_non_whitespace != Some('>') && chars.peek().copied() != Some('>') => {
                return true;
            }
            _ => {}
        }
        if !ch.is_whitespace() {
            previous_non_whitespace = Some(ch);
        }
    }

    quote.is_some() || escaped
}

#[cfg(any(not(windows), test))]
fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
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
