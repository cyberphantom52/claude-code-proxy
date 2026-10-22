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
    let home = tempfile::TempDir::new().unwrap();
    let codex = home.path().join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("models_cache.json"),
        json!({"models":[{"slug":"gpt-implicit-must-not-load"}]}).to_string(),
    )
    .unwrap();
    // This integration test owns its process and only uses synthetic metadata.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("CODEX_HOME", &codex);
        std::env::set_var("CCP_CONFIG_DIR", home.path());
        std::env::remove_var("CCP_CODEX_MODELS_FILE");
    }
    assert!(catalog_path().is_none());
    assert!(catalog_model("gpt-implicit-must-not-load").is_none());

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
    unsafe { std::env::set_var("CCP_CODEX_MODELS_FILE", &import) };
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
