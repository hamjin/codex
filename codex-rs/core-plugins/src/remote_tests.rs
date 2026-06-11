use super::*;
use pretty_assertions::assert_eq;

fn item(name: &str, display_name: &str) -> RecommendedPluginItem {
    RecommendedPluginItem {
        name: name.to_string(),
        status: None,
        installation_policy: None,
        release: RecommendedPluginRelease {
            display_name: display_name.to_string(),
        },
    }
}

#[test]
fn recommended_plugins_enabled_flag_selects_endpoint_or_legacy_mode() {
    let disabled: RecommendedPluginsResponse = serde_json::from_value(serde_json::json!({
        "enabled": false,
        "plugins": [{"name": "github", "release": {"display_name": "GitHub"}}]
    }))
    .expect("response should deserialize");
    assert_eq!(
        recommended_plugins_mode(disabled),
        RecommendedPluginsMode::Legacy
    );

    for response in [
        serde_json::json!({"plugins": []}),
        serde_json::json!({"enabled": null, "plugins": []}),
    ] {
        let response: RecommendedPluginsResponse =
            serde_json::from_value(response).expect("response should deserialize");
        assert_eq!(
            recommended_plugins_mode(response),
            RecommendedPluginsMode::Legacy
        );
    }

    let enabled: RecommendedPluginsResponse = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "plugins": []
    }))
    .expect("response should deserialize");
    assert_eq!(
        recommended_plugins_mode(enabled),
        RecommendedPluginsMode::Endpoint {
            plugins: Vec::new()
        }
    );
}

#[test]
fn recommended_plugins_are_validated_deduplicated_sorted_and_capped() {
    let mut plugins = (0..=52)
        .rev()
        .map(|index| item(&format!("plugin-{index:02}"), &format!("Plugin {index:02}")))
        .collect::<Vec<_>>();
    plugins.push(item("plugin-00", "Duplicate"));
    plugins.push(item("not/a/plugin", "Invalid"));
    plugins.push(RecommendedPluginItem {
        name: "disabled".to_string(),
        status: Some(PluginAvailability::DisabledByAdmin),
        installation_policy: Some(PluginInstallPolicy::Available),
        release: RecommendedPluginRelease {
            display_name: "Disabled".to_string(),
        },
    });
    plugins.push(RecommendedPluginItem {
        name: "not-available".to_string(),
        status: Some(PluginAvailability::Available),
        installation_policy: Some(PluginInstallPolicy::NotAvailable),
        release: RecommendedPluginRelease {
            display_name: "Not Available".to_string(),
        },
    });

    let mode = recommended_plugins_mode(RecommendedPluginsResponse {
        enabled: Some(true),
        plugins,
    });
    let RecommendedPluginsMode::Endpoint { plugins } = mode else {
        panic!("expected endpoint mode");
    };

    assert_eq!(plugins.len(), MAX_RECOMMENDED_PLUGINS);
    assert_eq!(
        plugins.first(),
        Some(&RecommendedPlugin {
            config_id: "plugin-00@openai-curated-remote".to_string(),
            display_name: "Plugin 00".to_string(),
        })
    );
    assert_eq!(
        plugins.last(),
        Some(&RecommendedPlugin {
            config_id: "plugin-49@openai-curated-remote".to_string(),
            display_name: "Plugin 49".to_string(),
        })
    );
}
