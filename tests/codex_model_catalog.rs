use claude_code_proxy::{
    providers::codex::{
        native::validate_native_request_model,
        translate::{
            model_allowlist::{
                assert_allowed_model, resolve_model_request, split_tier_suffix, uses_responses_lite,
            },
            model_catalog::{
                CatalogSnapshot, catalog_model, catalog_path, parse_catalog, with_catalog,
            },
        },
    },
    registry::Registry,
};
use serde_json::json;
use std::sync::Arc;

#[test]
fn models_do_not_depend_on_codex_cli_and_explicit_imports_remain_optional() {
    if std::env::var_os("CCP_TEST_MODEL_CATALOG").is_some() {
        check_model_catalog();
        return;
    }
    let home = tempfile::TempDir::new().unwrap();
    let codex = home.path().join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("models_cache.json"),
        json!({"models":[{"slug":"gpt-implicit-must-not-load"}]}).to_string(),
    )
    .unwrap();
    let import = home.path().join("explicit-models.json");
    std::fs::write(
        &import,
        json!({"models": [
            {"slug": "gpt-catalog-test", "use_responses_lite": true},
            {"slug": "gpt-hidden-test", "visibility": "hide"},
            {"slug": "gpt-subscription-only-test", "supported_in_api": false}
        ]})
        .to_string(),
    )
    .unwrap();
    for explicit_import in [false, true] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "models_do_not_depend_on_codex_cli_and_explicit_imports_remain_optional",
            ])
            .env_clear()
            .env("HOME", home.path())
            .env("CODEX_HOME", &codex)
            .env("CCP_CONFIG_DIR", home.path())
            .env("CCP_CODEX_MODEL_DISCOVERY", "0")
            .env("CCP_TEST_MODEL_CATALOG", "1");
        if explicit_import {
            command.env("CCP_CODEX_MODELS_FILE", &import);
        }
        assert!(command.status().unwrap().success());
    }
}

fn check_model_catalog() {
    assert!(catalog_model("gpt-implicit-must-not-load").is_none());
    if std::env::var_os("CCP_CODEX_MODELS_FILE").is_none() {
        assert!(catalog_path().is_none());
        return;
    }
    assert!(catalog_path().is_some());
    let registry = Registry::new();
    for model in [
        "gpt-catalog-test",
        "gpt-hidden-test",
        "gpt-subscription-only-test",
    ] {
        assert_allowed_model(model).unwrap();
        for name in [model.to_string(), format!("{model}-fast")] {
            assert_eq!(registry.provider_for_model(&name).unwrap().name(), "codex");
            assert_eq!(
                validate_native_request_model(&json!({"model": name})).unwrap(),
                name
            );
            assert_eq!(resolve_model_request(&name).model, model);
        }
        assert!(split_tier_suffix(&format!("{model}-fast")).is_some());
        assert!(split_tier_suffix(&format!("{model}-ultrafast")).is_none());
    }
    assert!(uses_responses_lite("gpt-catalog-test"));
    assert!(uses_responses_lite("gpt-6.1-sol"));
    let listed = registry.supported_models_for("codex");
    assert!(listed.contains(&"gpt-catalog-test-fast".to_string()));
    assert!(listed.contains(&"gpt-subscription-only-test".to_string()));
    assert!(!listed.contains(&"gpt-hidden-test".to_string()));

    // A Registry created before discovery must still use the operation's catalog.
    let discovered = Arc::new(CatalogSnapshot::new(parse_catalog(&serde_json::to_vec(&json!({"models": [
        {"slug":"gpt-discovered", "supported_in_api":false, "service_tiers":[{"id":"priority"}]},
        {"slug":"gpt-hidden-live", "visibility":"hide", "service_tiers":[]}
    ]})).unwrap()).unwrap(), true));
    with_catalog(discovered, || {
        assert!(registry.provider_for_model("gpt-discovered").is_some());
        assert!(registry.provider_for_model("gpt-discovered-fast").is_some());
        assert!(registry.provider_for_model("gpt-hidden-live").is_some());
        assert!(
            registry
                .provider_for_model("gpt-hidden-live-fast")
                .is_some()
        );
        assert!(registry.provider_for_model("gpt-catalog-test").is_some());
        assert!(registry.provider_for_model("gpt-6-sol").is_some());
        let names = registry.supported_models_for("codex");
        assert!(names.contains(&"gpt-discovered".to_string()));
        assert!(!names.contains(&"gpt-hidden-live".to_string()));
        assert!(!names.contains(&"gpt-6-sol".to_string()));
    });
    assert!(catalog_model("gpt-discovered").is_none());
}
