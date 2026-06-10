#![cfg(unix)]

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::create_shell_command_sse_response;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml_with_chatgpt_base_url;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time::timeout;

use super::analytics::mount_analytics_capture;
use super::analytics::wait_for_analytics_events;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PLUGIN_CONFIG_NAME: &str = "lifecycle@test";

#[tokio::test]
async fn plugin_script_emits_started_and_completed_lifecycle_analytics() -> Result<()> {
    let temp = TempDir::new()?;
    let codex_home = temp.path().join("codex-home");
    let working_directory = temp.path().join("workdir");
    std::fs::create_dir_all(&codex_home)?;
    std::fs::create_dir_all(&working_directory)?;

    let plugin_root = codex_home.join("plugins/cache/test/lifecycle/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::create_dir_all(plugin_root.join("scripts"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "lifecycle",
  "interface": {
    "developerName": "OpenAI"
  }
}"#,
    )?;
    let script_path = plugin_root.join("scripts/run.sh");
    std::fs::write(&script_path, "printf 'sensitive-script-output\\n'\n")?;

    let command = vec![
        "sh".to_string(),
        script_path.to_string_lossy().into_owned(),
        "secret-argument".to_string(),
    ];
    let server = create_mock_responses_server_sequence(vec![
        create_shell_command_sse_response(
            command,
            Some(&working_directory),
            Some(5_000),
            "plugin-script-call",
        )?,
        create_final_assistant_message_sse_response("done")?,
    ])
    .await;
    write_mock_responses_config_toml_with_chatgpt_base_url(
        &codex_home,
        &server.uri(),
        &server.uri(),
    )?;
    let config_path = codex_home.join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        &config_path,
        format!(
            r#"{config}
[features]
plugins = true
plugin_script_lifecycle_analytics = true

[plugins."{PLUGIN_CONFIG_NAME}"]
enabled = true
"#,
        ),
    )?;
    mount_analytics_capture(&server, &codex_home).await?;

    let isolated_home = codex_home.to_string_lossy();
    let mut mcp = TestAppServer::new_with_env(
        &codex_home,
        &[
            ("HOME", Some(isolated_home.as_ref())),
            ("USERPROFILE", Some(isolated_home.as_ref())),
        ],
    )
    .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_request = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_response)?;

    let turn_request = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "run the plugin script".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: Some(working_directory),
            sandbox_policy: Some(codex_app_server_protocol::SandboxPolicy::DangerFullAccess),
            ..Default::default()
        })
        .await?;
    let turn_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_request)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_response)?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let events = wait_for_analytics_events(
        &server,
        DEFAULT_READ_TIMEOUT,
        "codex_plugin_lifecycle_event",
        2,
    )
    .await?;
    let started = event_with_status(&events, "started")?;
    let completed = event_with_status(&events, "completed")?;
    let started_params = &started["event_params"];
    let completed_params = &completed["event_params"];

    assert_eq!(started_params["version"], 1);
    assert_eq!(started_params["thread_id"], thread.id);
    assert_eq!(started_params["session_id"], thread.session_id);
    assert_eq!(started_params["turn_id"], turn.id);
    assert_eq!(started_params["plugin_id"], PLUGIN_CONFIG_NAME);
    assert_eq!(started_params["script_path"], "scripts/run.sh");
    assert!(started_params.get("duration_ms").is_none());
    assert!(started_params.get("exit_code").is_none());
    assert_eq!(started_params["skill_id"], Value::Null);
    assert_eq!(
        completed_params["execution_id"],
        started_params["execution_id"]
    );
    assert!(completed_params["duration_ms"].as_u64().is_some());
    assert_eq!(completed_params["exit_code"], 0);

    let serialized_events = serde_json::to_string(&events)?;
    assert!(!serialized_events.contains("secret-argument"));
    assert!(!serialized_events.contains("sensitive-script-output"));
    assert!(!serialized_events.contains(plugin_root.to_string_lossy().as_ref()));

    Ok(())
}

#[tokio::test]
async fn plugin_script_emits_failed_lifecycle_analytics() -> Result<()> {
    let events = run_terminal_lifecycle_fixture("exit 7\n", false).await?;
    assert_eq!(events.len(), 2);

    let started = event_with_status(&events, "started")?;
    let failed = event_with_status(&events, "failed")?;
    assert_eq!(
        failed["event_params"]["execution_id"],
        started["event_params"]["execution_id"]
    );
    assert_eq!(failed["event_params"]["exit_code"], 7);
    assert!(failed["event_params"]["duration_ms"].as_u64().is_some());
    Ok(())
}

#[tokio::test]
async fn interrupted_plugin_script_emits_one_cancelled_lifecycle_event() -> Result<()> {
    let events = run_terminal_lifecycle_fixture("sleep 30\n", true).await?;
    assert_eq!(events.len(), 2);

    let started = event_with_status(&events, "started")?;
    let cancelled = event_with_status(&events, "cancelled")?;
    assert_eq!(
        cancelled["event_params"]["execution_id"],
        started["event_params"]["execution_id"]
    );
    assert!(cancelled["event_params"]["duration_ms"].as_u64().is_some());
    Ok(())
}

async fn run_terminal_lifecycle_fixture(script: &str, interrupt: bool) -> Result<Vec<Value>> {
    let temp = TempDir::new()?;
    let codex_home = temp.path().join("codex-home");
    let working_directory = temp.path().join("workdir");
    std::fs::create_dir_all(&codex_home)?;
    std::fs::create_dir_all(&working_directory)?;

    let plugin_root = codex_home.join("plugins/cache/test/lifecycle/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::create_dir_all(plugin_root.join("scripts"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{
  "name": "lifecycle",
  "interface": {
    "developerName": "OpenAI"
  }
}"#,
    )?;
    let script_path = plugin_root.join("scripts/run.sh");
    std::fs::write(&script_path, script)?;

    let mut responses = vec![create_shell_command_sse_response(
        vec!["sh".to_string(), script_path.to_string_lossy().into_owned()],
        Some(&working_directory),
        Some(60_000),
        "plugin-script-call",
    )?];
    if !interrupt {
        responses.push(create_final_assistant_message_sse_response("done")?);
    }
    let server = create_mock_responses_server_sequence(responses).await;
    write_mock_responses_config_toml_with_chatgpt_base_url(
        &codex_home,
        &server.uri(),
        &server.uri(),
    )?;
    let config_path = codex_home.join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        &config_path,
        format!(
            r#"{config}
[features]
plugins = true
plugin_script_lifecycle_analytics = true

[plugins."{PLUGIN_CONFIG_NAME}"]
enabled = true
"#,
        ),
    )?;
    mount_analytics_capture(&server, &codex_home).await?;

    let isolated_home = codex_home.to_string_lossy();
    let mut mcp = TestAppServer::new_with_env(
        &codex_home,
        &[
            ("HOME", Some(isolated_home.as_ref())),
            ("USERPROFILE", Some(isolated_home.as_ref())),
        ],
    )
    .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_request = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_request)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_response)?;

    let turn_request = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "run the plugin script".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: Some(working_directory),
            sandbox_policy: Some(codex_app_server_protocol::SandboxPolicy::DangerFullAccess),
            ..Default::default()
        })
        .await?;
    let turn_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_request)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_response)?;

    if interrupt {
        wait_for_analytics_events(
            &server,
            DEFAULT_READ_TIMEOUT,
            "codex_plugin_lifecycle_event",
            1,
        )
        .await?;
        let interrupt_request = mcp
            .send_turn_interrupt_request(TurnInterruptParams {
                thread_id: thread.id,
                turn_id: turn.id,
            })
            .await?;
        let interrupt_response: JSONRPCResponse = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(interrupt_request)),
        )
        .await??;
        let _: TurnInterruptResponse = to_response(interrupt_response)?;
    }

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    wait_for_analytics_events(
        &server,
        DEFAULT_READ_TIMEOUT,
        "codex_plugin_lifecycle_event",
        2,
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    wait_for_analytics_events(
        &server,
        DEFAULT_READ_TIMEOUT,
        "codex_plugin_lifecycle_event",
        2,
    )
    .await
}

fn event_with_status<'a>(events: &'a [Value], status: &str) -> Result<&'a Value> {
    events
        .iter()
        .find(|event| event["event_params"]["status"] == status)
        .ok_or_else(|| anyhow::anyhow!("missing plugin lifecycle status {status}"))
}
