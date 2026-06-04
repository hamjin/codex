#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use tokio::io::AsyncBufReadExt;
#[cfg(target_os = "macos")]
use tokio::process::Child;
#[cfg(target_os = "macos")]
use tokio::task::JoinHandle;

#[cfg(target_os = "macos")]
use self::pid_tracker::PidTracker;

#[cfg(target_os = "macos")]
const LOG_STREAM_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(target_os = "macos")]
const LOG_STREAM_FLUSH_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);
#[cfg(target_os = "macos")]
const LOG_STREAM_READY_PREFIX: &str = "Filtering the log data using ";

#[cfg(target_os = "macos")]
mod pid_tracker;

/// A unique macOS Seatbelt sandbox denial emitted by a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDenial {
    pub name: String,
    pub capability: String,
}

/// Best-effort collector for macOS Seatbelt denials emitted by a process tree.
pub struct DenialLogger {
    #[cfg(target_os = "macos")]
    log_stream: Child,
    #[cfg(target_os = "macos")]
    pid_tracker: Option<PidTracker>,
    #[cfg(target_os = "macos")]
    log_reader: Option<JoinHandle<Vec<u8>>>,
    #[cfg(target_os = "macos")]
    stderr_reader: Option<JoinHandle<()>>,
}

impl DenialLogger {
    /// Starts collecting Seatbelt denial log messages.
    #[cfg(target_os = "macos")]
    pub async fn new() -> Option<Self> {
        let mut log_stream = start_log_stream()?;
        let stdout = log_stream.stdout.take()?;
        let stderr = log_stream.stderr.take()?;
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(1);
        let stdout_ready_tx = ready_tx.clone();
        let log_reader = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut logs = Vec::new();
            let mut chunk = Vec::new();
            loop {
                match reader.read_until(b'\n', &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if chunk.starts_with(LOG_STREAM_READY_PREFIX.as_bytes()) {
                            let _ = stdout_ready_tx.try_send(());
                        } else {
                            logs.extend_from_slice(&chunk);
                        }
                        chunk.clear();
                    }
                }
            }
            logs
        });

        let stderr_reader = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            while reader
                .read_line(&mut line)
                .await
                .is_ok_and(|bytes_read| bytes_read > 0)
            {
                if line.starts_with(LOG_STREAM_READY_PREFIX) {
                    let _ = ready_tx.try_send(());
                }
                line.clear();
            }
        });

        let ready = tokio::time::timeout(LOG_STREAM_READY_TIMEOUT, ready_rx.recv())
            .await
            .is_ok_and(|result| result.is_some())
            && log_stream.try_wait().ok().flatten().is_none();
        if !ready {
            let _ = log_stream.kill().await;
            let _ = log_stream.wait().await;
            log_reader.abort();
            stderr_reader.abort();
            return None;
        }

        Some(Self {
            log_stream,
            pid_tracker: None,
            log_reader: Some(log_reader),
            stderr_reader: Some(stderr_reader),
        })
    }

    /// Returns no logger on platforms without macOS Seatbelt.
    #[cfg(not(target_os = "macos"))]
    pub async fn new() -> Option<Self> {
        None
    }

    /// Starts tracking the process tree rooted at `child_pid`.
    #[cfg(target_os = "macos")]
    pub fn on_child_pid(&mut self, child_pid: Option<u32>) {
        if let Some(root_pid) = child_pid {
            self.pid_tracker = PidTracker::new(root_pid as i32);
        }
    }

    /// Does nothing on platforms without macOS Seatbelt.
    #[cfg(not(target_os = "macos"))]
    pub fn on_child_pid(&mut self, child_pid: Option<u32>) {
        let _ = child_pid;
    }

    /// Stops collection and returns unique denials from the tracked process tree.
    #[cfg(target_os = "macos")]
    pub async fn finish(mut self) -> Vec<SandboxDenial> {
        let pid_set = match self.pid_tracker {
            Some(tracker) => tracker.stop().await,
            None => Default::default(),
        };

        if !pid_set.is_empty() {
            tokio::time::sleep(LOG_STREAM_FLUSH_GRACE_PERIOD).await;
        }
        let _ = self.log_stream.kill().await;
        let _ = self.log_stream.wait().await;
        if let Some(handle) = self.stderr_reader.take() {
            let _ = handle.await;
        }

        let logs_bytes = match self.log_reader.take() {
            Some(handle) => handle.await.unwrap_or_default(),
            None => Vec::new(),
        };
        if pid_set.is_empty() {
            return Vec::new();
        }
        let logs = String::from_utf8_lossy(&logs_bytes);

        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut denials: Vec<SandboxDenial> = Vec::new();
        for line in logs.lines() {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line)
                && let Some(msg) = json.get("eventMessage").and_then(|v| v.as_str())
                && let Some((pid, name, capability)) = parse_message(msg)
                && pid_set.contains(&pid)
                && seen.insert((name.clone(), capability.clone()))
            {
                denials.push(SandboxDenial { name, capability });
            }
        }
        denials
    }

    /// Returns no denials on platforms without macOS Seatbelt.
    #[cfg(not(target_os = "macos"))]
    pub async fn finish(self) -> Vec<SandboxDenial> {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
fn start_log_stream() -> Option<Child> {
    use std::process::Stdio;

    const PREDICATE: &str = r#"(((processID == 0) AND (senderImagePath CONTAINS "/Sandbox")) OR (subsystem == "com.apple.sandbox.reporting"))"#;

    tokio::process::Command::new("log")
        .args(["stream", "--style", "ndjson", "--predicate", PREDICATE])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()
}

#[cfg(target_os = "macos")]
fn parse_message(msg: &str) -> Option<(i32, String, String)> {
    // Example message:
    // Sandbox: processname(1234) deny(1) capability-name args...
    static RE: std::sync::OnceLock<regex_lite::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        #[expect(clippy::unwrap_used)]
        regex_lite::Regex::new(r"^Sandbox:\s*(.+?)\((\d+)\)\s+deny\(.*?\)\s*(.+)$").unwrap()
    });

    let (_, [name, pid_str, capability]) = re.captures(msg)?.extract();
    let pid = pid_str.trim().parse::<i32>().ok()?;
    Some((pid, name.to_string(), capability.to_string()))
}

/// Formats denials for appending to command output.
pub fn format_sandbox_denials(denials: &[SandboxDenial]) -> Option<Vec<u8>> {
    if denials.is_empty() {
        return None;
    }

    let mut formatted = String::from("\n=== Sandbox denials ===\n");
    for SandboxDenial { name, capability } in denials {
        formatted.push_str(&format!("({name}) {capability}\n"));
    }
    Some(formatted.into_bytes())
}

#[cfg(all(test, target_os = "macos"))]
#[path = "seatbelt_denials_tests.rs"]
mod tests;
