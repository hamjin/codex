#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::Result;
use codex_config::types::ToolSuggestDisabledTool;
use codex_config::types::ToolSuggestDiscoverable;
use codex_config::types::ToolSuggestDiscoverableType;
use codex_core::config::Config;
use codex_core_plugins::RecommendedPluginsMode;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_models_manager::bundled_models_response;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
const LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME: &str = "list_available_plugins_to_install";
const REQUEST_PLUGIN_INSTALL_TOOL_NAME: &str = "request_plugin_install";
const DISCOVERABLE_GMAIL_ID: &str = "connector_68df038e0ba48191908c8434991bbac2";

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn function_tool<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| {
            tools
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        })
}

fn configure_apps_without_search_tool(config: &mut Config, apps_base_url: &str) {
    for feature in [
        Feature::Apps,
        Feature::Plugins,
        Feature::RemotePlugin,
        Feature::ToolSuggest,
    ] {
        config
            .features
            .enable(feature)
            .expect("test config should allow feature update");
    }
    let mut model_catalog = bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));
    let model = model_catalog
        .models
        .iter_mut()
        .find(|model| model.slug == "gpt-5.4")
        .expect("gpt-5.4 exists in bundled models.json");
    config.chatgpt_base_url = apps_base_url.to_string();
    config.model = Some("gpt-5.4".to_string());
    config.tool_suggest.discoverables = vec![ToolSuggestDiscoverable {
        kind: ToolSuggestDiscoverableType::Connector,
        id: DISCOVERABLE_GMAIL_ID.to_string(),
    }];
    model.supports_search_tool = false;
    config.model_catalog = Some(model_catalog);
}

async fn refresh_recommendations(test: &TestCodex) -> Result<RecommendedPluginsMode> {
    let auth = test.thread_manager.auth_manager().auth().await;
    Ok(test
        .thread_manager
        .plugins_manager()
        .refresh_recommended_plugins_for_config(&test.config.plugins_config_input(), auth.as_ref())
        .await?)
}

async fn mount_recommendations(server: &wiremock::MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/ps/plugins/suggested"))
        .and(query_param("scope", "GLOBAL"))
        .respond_with(response)
        .mount(server)
        .await;
}

fn assert_legacy_tools(body: &Value) {
    let tools = tool_names(body);
    assert!(!tools.iter().any(|name| name == TOOL_SEARCH_TOOL_NAME));
    assert!(
        tools
            .iter()
            .any(|name| name == LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME),
        "legacy mode should expose {LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME}: {tools:?}"
    );
    assert!(
        tools
            .iter()
            .any(|name| name == REQUEST_PLUGIN_INSTALL_TOOL_NAME),
        "legacy mode should expose {REQUEST_PLUGIN_INSTALL_TOOL_NAME}: {tools:?}"
    );
    let description = function_tool(body, REQUEST_PLUGIN_INSTALL_TOOL_NAME)
        .and_then(|tool| tool.get("description"))
        .and_then(Value::as_str)
        .expect("request tool description");
    assert!(description.contains(LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME));
    assert!(!description.contains("developer recommendations"));
}

async fn build_test(
    server: &wiremock::MockServer,
    apps_server: &AppsTestServer,
) -> Result<TestCodex> {
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config({
            let apps_base_url = apps_server.chatgpt_base_url.clone();
            move |config| configure_apps_without_search_tool(config, apps_base_url.as_str())
        });
    builder.build(server).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_false_preserves_legacy_workflow() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({"enabled": false, "plugins": []})),
    )
    .await;
    let call_id = "list-installable-tools";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(call_id, LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME, "{}"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = build_test(&server, &apps_server).await?;
    assert_eq!(
        refresh_recommendations(&test).await?,
        RecommendedPluginsMode::Legacy
    );

    test.submit_turn_with_approval_and_permission_profile(
        "list tools",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let request = &requests[0];
    assert!(
        !request
            .message_input_texts("developer")
            .join("\n")
            .contains("<recommended_plugins>")
    );
    assert_legacy_tools(&request.body_json());
    let output = requests[1]
        .function_call_output_text(call_id)
        .expect("list tool output");
    let output: Value = serde_json::from_str(&output)?;
    assert!(output["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["id"] == DISCOVERABLE_GMAIL_ID && tool["tool_type"] == "connector")
    }));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_fetch_preserves_legacy_workflow() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(&server, ResponseTemplate::new(500)).await;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let test = build_test(&server, &apps_server).await?;
    assert!(refresh_recommendations(&test).await.is_err());

    test.submit_turn("list tools").await?;
    let request = mock.single_request();
    assert!(
        !request
            .message_input_texts("developer")
            .join("\n")
            .contains("<recommended_plugins>")
    );
    assert_legacy_tools(&request.body_json());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_mode_injects_candidates_hides_list_and_rejects_invented_ids() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "enabled": true,
            "plugins": [
                {
                    "name": "google-calendar",
                    "status": "ENABLED",
                    "installation_policy": "AVAILABLE",
                    "release": {"display_name": "Google Calendar"}
                },
                {
                    "name": "github",
                    "status": "ENABLED",
                    "installation_policy": "AVAILABLE",
                    "release": {"display_name": "GitHub"}
                }
            ]
        })),
    )
    .await;
    let call_id = "invented-plugin";
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    call_id,
                    REQUEST_PLUGIN_INSTALL_TOOL_NAME,
                    &serde_json::to_string(&json!({
                        "tool_type": "plugin",
                        "action_type": "install",
                        "tool_id": "invented@openai-curated-remote",
                        "suggest_reason": "Try this"
                    }))?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = build_test(&server, &apps_server).await?;
    assert!(matches!(
        refresh_recommendations(&test).await?,
        RecommendedPluginsMode::Endpoint { .. }
    ));

    test.submit_turn("suggest a plugin").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let developer_message = requests[0].message_input_texts("developer").join("\n");
    assert!(developer_message.contains("<recommended_plugins>"));
    assert!(developer_message.contains("- GitHub (plugin id: github@openai-curated-remote)"));
    assert!(
        developer_message
            .contains("- Google Calendar (plugin id: google-calendar@openai-curated-remote)")
    );
    let body = requests[0].body_json();
    let tools = tool_names(&body);
    assert!(
        !tools
            .iter()
            .any(|name| name == LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME)
    );
    assert!(
        tools
            .iter()
            .any(|name| name == REQUEST_PLUGIN_INSTALL_TOOL_NAME)
    );
    let request_tool = function_tool(&body, REQUEST_PLUGIN_INSTALL_TOOL_NAME)
        .expect("request_plugin_install tool");
    assert!(
        request_tool
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|description| description.contains("developer recommendations"))
    );
    assert_eq!(
        request_tool
            .pointer("/parameters/required")
            .and_then(Value::as_array)
            .map(Vec::as_slice),
        Some(
            &[
                Value::String("tool_type".to_string()),
                Value::String("action_type".to_string()),
                Value::String("tool_id".to_string()),
                Value::String("suggest_reason".to_string()),
            ][..]
        )
    );
    let output = requests[1].function_call_output(call_id);
    assert!(
        output
            .get("output")
            .and_then(Value::as_str)
            .is_some_and(|output| output.contains("developer recommendations"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_mode_with_no_eligible_candidates_exposes_no_suggestion_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    mount_recommendations(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "enabled": true,
            "plugins": [{
                "name": "google-calendar",
                "release": {"display_name": "Google Calendar"}
            }]
        })),
    )
    .await;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config({
            let apps_base_url = apps_server.chatgpt_base_url.clone();
            move |config| {
                configure_apps_without_search_tool(config, apps_base_url.as_str());
                config.tool_suggest.disabled_tools = vec![ToolSuggestDisabledTool::plugin(
                    "google-calendar@openai-curated-remote",
                )];
            }
        });
    let test = builder.build(&server).await?;
    refresh_recommendations(&test).await?;

    test.submit_turn("list tools").await?;

    let request = mock.single_request();
    assert!(
        !request
            .message_input_texts("developer")
            .join("\n")
            .contains("<recommended_plugins>")
    );
    let tools = tool_names(&request.body_json());
    assert!(
        !tools
            .iter()
            .any(|name| name == LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME)
    );
    assert!(
        !tools
            .iter()
            .any(|name| name == REQUEST_PLUGIN_INSTALL_TOOL_NAME)
    );
    Ok(())
}
