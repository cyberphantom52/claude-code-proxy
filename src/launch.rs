use std::{collections::BTreeMap, ffi::OsString, process::Command, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::{
    config,
    providers::codex::{
        client::CodexHttpClient,
        models,
        translate::{
            model_allowlist::{
                is_valid_model_for_codex, resolve_model_request_with_config_override,
            },
            model_catalog::{CatalogModel, CatalogSnapshot, with_catalog},
        },
    },
    registry::normalize_incoming_model,
};

#[derive(Debug, Args)]
pub struct LaunchArgs {
    /// Main model (defaults to an accessible catalog model)
    #[arg(long)]
    pub model: Option<String>,
    /// Small/fast model (defaults to an accessible catalog model)
    #[arg(long)]
    pub small_model: Option<String>,
    /// Safe context capacity, required when model metadata has no window
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub context_window: Option<u64>,
    /// URL of an already running proxy
    #[arg(long)]
    pub proxy_url: Option<reqwest::Url>,
    /// Print the child environment as JSON without starting Claude
    #[arg(long)]
    pub print_env: bool,
    /// Arguments passed unchanged to Claude Code
    #[arg(last = true)]
    pub claude_args: Vec<OsString>,
}

pub async fn run(args: LaunchArgs) -> Result<i32> {
    if args.claude_args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == "--model" || arg.starts_with("--model="))
    }) {
        bail!(
            "pass --model to `launch` before `--`, so model access and context limits can be validated"
        );
    }
    let client = CodexHttpClient::new();
    let context = client.prepare_model_context().await;
    let catalog = context.catalog();
    let env = models::with_model_context(context, async {
        launch_env(&args, catalog, config::codex_model().as_deref())
    })
    .await?;
    if args.print_env {
        println!("{}", serde_json::to_string_pretty(&env)?);
        return Ok(0);
    }
    let status = Command::new("claude")
        .args(&args.claude_args)
        .envs(env)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("could not start Claude Code; ensure `claude` is on PATH")?;
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
    }
    #[cfg(not(unix))]
    Ok(status.code().unwrap_or(1))
}

fn launch_env(
    args: &LaunchArgs,
    catalog: Arc<CatalogSnapshot>,
    model_override: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    with_catalog(catalog.clone(), || {
        let main = select_model(
            args.model.as_deref(),
            &catalog,
            &["gpt-6-astra"],
            args.context_window.is_some(),
        )?;
        let small = select_model(
            args.small_model.as_deref(),
            &catalog,
            &["gpt-6-luna", "gpt-5.4-mini", "gpt-5.6-luna"],
            args.context_window.is_some(),
        )?;
        let mut windows = Vec::new();
        let mut compacts = Vec::new();
        for model in [&main, &small] {
            let normalized = normalize_incoming_model(model);
            if !is_valid_model_for_codex(&normalized) {
                bail!(
                    "invalid model ID {model:?}: expected a non-empty ID without whitespace/control characters (maximum 256 bytes)"
                );
            }
            let resolved = resolve_model_request_with_config_override(&normalized, false);
            if let Some(value) = model_override.filter(|value| !value.is_empty()) {
                let forced = resolve_model_request_with_config_override(value, false);
                if forced.model != resolved.model
                    || (forced.service_tier.is_some()
                        && forced.service_tier != resolved.service_tier)
                {
                    bail!(
                        "CCP_CODEX_MODEL/codex.model override {value:?} conflicts with launch model {model:?}; remove it to select independent models"
                    );
                }
            }
            let metadata = catalog
                .models
                .iter()
                .find(|entry| entry.slug == resolved.model);
            let advertised = metadata.and_then(CatalogModel::context_window_tokens);
            let upper_bound = advertised
                .or_else(|| metadata.and_then(|model| model.max_context_window.filter(|n| *n > 0)));
            if let (Some(requested), Some(limit)) = (args.context_window, upper_bound)
                && requested > limit
            {
                bail!("--context-window {requested} exceeds {model:?} advertised context {limit}");
            }
            let window = match (advertised, args.context_window) {
                (Some(capacity), requested) => requested.unwrap_or(capacity),
                (None, Some(requested)) => requested,
                (None, None) => bail!(
                    "context metadata unavailable for {model:?}; pass --context-window <tokens>"
                ),
            };
            let compact = ((u128::from(window) * 90) / 100) as u64;
            let budget = match metadata.and_then(CatalogModel::usable_context_tokens) {
                Some(usable) => window.min(usable),
                None if advertised.is_some() => {
                    bail!("no usable context capacity advertised for {model:?}")
                }
                None => window,
            };
            let compact = metadata
                .and_then(CatalogModel::auto_compact_tokens)
                .or_else(|| metadata.and_then(|model| model.auto_compact_token_limit))
                .map_or(compact, |limit| compact.min(limit))
                .min(budget)
                .min(1_000_000);
            if compact < 100_000 {
                bail!(
                    "Claude Code requires CLAUDE_CODE_AUTO_COMPACT_WINDOW >= 100000 tokens; {model:?} only permits a safe window of {compact}. Choose a model/context budget with a larger window"
                );
            }
            windows.push(budget);
            compacts.push(compact);
        }
        let url = args.proxy_url.clone().unwrap_or_else(|| {
            format!("http://127.0.0.1:{}", config::port())
                .parse()
                .expect("local proxy URL")
        });
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("--proxy-url must be an HTTP(S) URL without credentials, query or fragment");
        }
        Ok(BTreeMap::from([
            (
                "ANTHROPIC_BASE_URL".into(),
                url.to_string().trim_end_matches('/').into(),
            ),
            ("ANTHROPIC_AUTH_TOKEN".into(), "unused".into()),
            ("ANTHROPIC_MODEL".into(), main),
            ("ANTHROPIC_SMALL_FAST_MODEL".into(), small),
            (
                "ANTHROPIC_DEFAULT_FABLE_MODEL".into(),
                "claude-fable-5-1".into(),
            ),
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL".into(),
                "claude-opus-5-5".into(),
            ),
            (
                "ANTHROPIC_DEFAULT_SONNET_MODEL".into(),
                "claude-sonnet-5-5".into(),
            ),
            (
                "ANTHROPIC_DEFAULT_HAIKU_MODEL".into(),
                "claude-haiku-5-5".into(),
            ),
            (
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(),
                windows.into_iter().min().unwrap().to_string(),
            ),
            (
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(),
                compacts.into_iter().min().unwrap().to_string(),
            ),
            ("ENABLE_TOOL_SEARCH".into(), "true".into()),
            (
                "CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS".into(),
                "1".into(),
            ),
            ("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK".into(), "1".into()),
            (
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".into(),
                "1".into(),
            ),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
                "1".into(),
            ),
            (
                "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK".into(),
                "1".into(),
            ),
        ]))
    })
}

fn select_model(
    requested: Option<&str>,
    catalog: &CatalogSnapshot,
    defaults: &[&str],
    allow_missing_context: bool,
) -> Result<String> {
    if let Some(model) = requested {
        return Ok(model.to_owned());
    }
    let mut accessible: Vec<_> = catalog
        .models
        .iter()
        .filter(|model| {
            model.listed
                && (allow_missing_context
                    || (model
                        .usable_context_tokens()
                        .is_some_and(|limit| limit >= 100_000)
                        && model
                            .auto_compact_tokens()
                            .is_some_and(|limit| limit >= 100_000)))
        })
        .collect();
    for default in defaults {
        if accessible.iter().any(|model| model.slug == *default) {
            return Ok((*default).into());
        }
    }
    accessible.sort_by_key(|model| (model.priority.unwrap_or(i64::MAX), &model.slug));
    accessible
        .first()
        .map(|model| model.slug.clone())
        .context("no listed model with usable context metadata; select explicit --model/--small-model and --context-window, or authenticate with `codex auth login`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::codex::translate::model_catalog::parse_catalog;

    #[test]
    fn max_only_metadata_requires_an_explicit_bounded_context_window() {
        let catalog = Arc::new(CatalogSnapshot::new(
            vec![CatalogModel {
                slug: "gpt-maxonly".into(),
                max_context_window: Some(872000),
                ..CatalogModel::default()
            }],
            true,
        ));
        let mut args = LaunchArgs {
            model: Some("gpt-maxonly".into()),
            small_model: Some("gpt-maxonly".into()),
            context_window: None,
            proxy_url: None,
            print_env: true,
            claude_args: vec![],
        };
        assert!(
            launch_env(&args, catalog.clone(), None)
                .unwrap_err()
                .to_string()
                .contains("--context-window")
        );
        args.context_window = Some(200000);
        assert_eq!(
            launch_env(&args, catalog.clone(), None).unwrap()["CLAUDE_CODE_MAX_CONTEXT_TOKENS"],
            "200000"
        );
        args.context_window = Some(900000);
        assert!(launch_env(&args, catalog, None).is_err());
    }

    #[test]
    fn launch_uses_active_window_and_conservative_small_model_budget() {
        let catalog = Arc::new(CatalogSnapshot::new(parse_catalog(br#"{"models":[
            {"slug":"gpt-6-astra","context_window":272000,"max_context_window":872000,"supported_in_api":true,"visibility":"list"},
            {"slug":"gpt-6-luna","context_window":200000,"auto_compact_token_limit":150000,"supported_in_api":true,"visibility":"list"}
        ]}"#).unwrap(), true));
        let mut args = LaunchArgs {
            model: Some("gpt-6-astra[1m]".into()),
            small_model: None,
            context_window: None,
            proxy_url: Some("http://localhost:18765".parse().unwrap()),
            print_env: true,
            claude_args: vec![],
        };
        let env = launch_env(&args, catalog.clone(), None).unwrap();
        assert_eq!(env["ANTHROPIC_MODEL"], "gpt-6-astra[1m]");
        assert_eq!(env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "190000");
        assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "150000");
        assert!(launch_env(&args, catalog.clone(), Some("gpt-6-astra")).is_err());
        args.model = Some("gpt-unavailable".into());
        assert!(launch_env(&args, catalog.clone(), None).is_err());
        args.context_window = Some(200000);
        assert_eq!(
            launch_env(&args, catalog.clone(), None).unwrap()["ANTHROPIC_MODEL"],
            "gpt-unavailable"
        );
        args.model = Some("gpt-6-astra".into());
        args.context_window = Some(100000);
        assert!(
            launch_env(&args, catalog.clone(), None)
                .unwrap_err()
                .to_string()
                .contains("AUTO_COMPACT_WINDOW")
        );
        args.context_window = Some(900000);
        assert!(launch_env(&args, catalog, None).is_err());
    }
}
