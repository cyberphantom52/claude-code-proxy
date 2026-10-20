use claude_code_proxy::{
    config::AliasProvider,
    providers::codex::{
        native::validate_native_request_model,
        translate::model_allowlist::{
            assert_allowed_model, resolve_model_request, split_tier_suffix, uses_responses_lite,
        },
    },
    registry::Registry,
};
use serde_json::json;

#[test]
fn catalog_models_route_with_fast_tiers_and_preserve_builtins() {
    let home = tempfile::TempDir::new().unwrap();
    std::fs::write(
        home.path().join("models_cache.json"),
        json!({"models": [
            {"slug": "gpt-catalog-test", "use_responses_lite": true},
            {"slug": "gpt-hidden-test", "visibility": "hide"},
            {"slug": "gpt-unavailable-test", "supported_in_api": false}
        ]})
        .to_string(),
    )
    .unwrap();
    // This integration test has its own process and only reads synthetic model data.
    unsafe { std::env::set_var("CODEX_HOME", home.path()) };
    let registry = Registry::new(AliasProvider::Codex);
    for model in ["gpt-catalog-test", "gpt-hidden-test"] {
        assert_allowed_model(model).unwrap();
        for name in [model.to_string(), format!("{model}-fast")] {
            assert_eq!(
                registry.provider_for_model(&name, None).unwrap().name(),
                "codex"
            );
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
    assert!(split_tier_suffix("gpt-6.1-sol-ultrafast").is_some());
    assert!(assert_allowed_model("gpt-unavailable-test").is_err());
    assert!(
        registry
            .provider_for_model("gpt-unavailable-test", None)
            .is_none()
    );
    let listed = registry.supported_models_for("codex");
    assert!(listed.contains(&"gpt-catalog-test-fast".to_string()));
    assert!(!listed.contains(&"gpt-hidden-test".to_string()));
}
