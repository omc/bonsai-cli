//! `doctor`, `version` and `completion`.

use std::sync::LazyLock;

use anyhow::Result;
use bonsai_api::{ApiRoot, Response};
use bonsai_core::auth::TokenSource;
use clap::CommandFactory;
use jiff::Timestamp;
use owo_colors::OwoColorize;
use serde_json::{Value, json};

use super::{Ctx, chain, quick_retry};
use crate::cli::{Cli, CompletionArgs};
use crate::report::{is_bare_404, no_api_at};
use crate::sanitize::sanitize;

/// The most the clock may be off by before `doctor` complains.
const MAX_SKEW_SECS: i64 = 60;

const CONTRACT: &str = include_str!("../../../../openapi/v1.yml");

/// The vendored contract's `info.version`, read without a YAML parser so the binary
/// doesn't carry `bonsai-contract`'s schema validator.
static CONTRACT_VERSION: LazyLock<String> = LazyLock::new(|| {
    CONTRACT
        .lines()
        .skip_while(|l| l.trim_end() != "info:")
        .skip(1)
        .take_while(|l| l.is_empty() || l.starts_with(' '))
        .find_map(|l| l.trim().strip_prefix("version:"))
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_owned())
        .expect("openapi/v1.yml has info.version")
});

/// The `/v1` contract version this CLI was built against.
pub fn contract_version() -> &'static str {
    &CONTRACT_VERSION
}

pub async fn version(ctx: &Ctx) -> Result<()> {
    let cli = env!("CARGO_PKG_VERSION");
    let api = match ctx.settings() {
        Ok(settings) => {
            let client = ctx.client_with(&settings, None, quick_retry())?;
            match client.get_api_root().await {
                Ok(root) => Some((root, settings.api_url)),
                Err(e) if is_bare_404(&e) => {
                    ctx.out
                        .info(&format!("{} (HTTP 404)", no_api_at(&settings.api_url)));
                    None
                }
                Err(e) => {
                    ctx.out.info(&format!(
                        "Couldn't reach the API at {}: {}",
                        settings.api_url,
                        chain(e)
                    ));
                    None
                }
            }
        }
        Err(e) => {
            ctx.out.info(&format!("Not asking the API: {e:#}"));
            None
        }
    };

    if ctx.out.is_json() {
        ctx.out.json(&json!({
            "cli": cli,
            "contract": contract_version(),
            "api": api.as_ref().map_or(Value::Null, |(root, _)| root.raw.clone()),
        }))?;
        return Ok(());
    }
    ctx.out.data_line(&format!("bonsai {cli}"))?;
    ctx.out
        .data_line(&format!("contract {}", contract_version()))?;
    if let Some((root, url)) = api {
        ctx.out.data_line(&format!(
            "api {} ({url})",
            sanitize(&root.value.version, false)
        ))?;
    }
    Ok(())
}

/// One `doctor` line. `ok: None` is a check that couldn't run.
struct Check {
    name: &'static str,
    ok: Option<bool>,
    detail: String,
}

impl Check {
    fn new(name: &'static str, ok: Option<bool>, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok,
            detail: detail.into(),
        }
    }
}

pub async fn doctor(ctx: &Ctx) -> Result<()> {
    let settings = ctx.settings()?;
    let anon = ctx.client_with(&settings, None, quick_retry())?;
    let mut checks = Vec::new();

    // A bare 404: something answers, but it isn't Bonsai's /v1.
    let mut no_api = false;
    let root: Option<Response<ApiRoot>> = match anon.get_api_root().await {
        Ok(root) => {
            checks.push(Check::new(
                "API reachable",
                Some(true),
                format!("{} ({})", settings.api_url, root.value.name),
            ));
            Some(root)
        }
        Err(e) => {
            no_api = is_bare_404(&e);
            let detail = if no_api {
                format!("{} (HTTP 404)", no_api_at(&settings.api_url))
            } else {
                format!("{}: {}", settings.api_url, chain(e))
            };
            checks.push(Check::new("API reachable", Some(false), detail));
            None
        }
    };

    checks.push(match ctx.token(&settings).await {
        // Asking it about the token would only get the same 404.
        _ if no_api => Check::new("Token valid", None, "skipped: no Bonsai /v1 API there"),
        Err(e) => Check::new("Token valid", Some(false), format!("{e:#}")),
        Ok((token, source)) => {
            let client = ctx.client_with(&settings, Some(token), quick_retry())?;
            let from = match source {
                TokenSource::Env => "BONSAI_TOKEN",
                TokenSource::Keychain => "the keychain",
                TokenSource::File => "credentials.toml",
            };
            match client.get_account().await {
                Ok(a) => Check::new(
                    "Token valid",
                    Some(true),
                    format!("{} ({}), from {from}", a.value.name, a.value.id),
                ),
                Err(e) => Check::new(
                    "Token valid",
                    Some(false),
                    format!("from {from}: {}", chain(e)),
                ),
            }
        }
    });

    checks.push(match &root {
        None => Check::new("Contract version", None, "skipped: the API isn't reachable"),
        Some(r) if r.value.version == contract_version() => Check::new(
            "Contract version",
            Some(true),
            contract_version().to_owned(),
        ),
        Some(r) => Check::new(
            "Contract version",
            Some(false),
            format!(
                "this CLI was built for {}, the API is {}",
                contract_version(),
                r.value.version
            ),
        ),
    });

    checks.push(match root.as_ref().map(|r| r.date) {
        None => Check::new("Clock", None, "skipped: the API isn't reachable"),
        Some(None) => Check::new("Clock", None, "skipped: the API sent no Date header"),
        Some(Some(date)) => {
            let skew = (Timestamp::now().as_second() - date.as_second()).abs();
            if skew < MAX_SKEW_SECS {
                Check::new("Clock", Some(true), format!("{skew}s off the API's"))
            } else {
                Check::new(
                    "Clock",
                    Some(false),
                    format!("{skew}s off the API's; --since ranges will be wrong"),
                )
            }
        }
    });

    let failed = checks.iter().filter(|c| c.ok == Some(false)).count();
    if ctx.out.is_json() {
        let list: Vec<Value> = checks
            .iter()
            .map(|c| json!({"name": c.name, "ok": c.ok, "detail": c.detail}))
            .collect();
        ctx.out.json(&json!({ "checks": list }))?;
    } else {
        let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
        for c in &checks {
            let mark = match c.ok {
                Some(true) => "✓".green().to_string(),
                Some(false) => "✗".red().to_string(),
                None => "-".dimmed().to_string(),
            };
            ctx.out.styled_line(&format!(
                "{mark} {:width$}  {}",
                c.name,
                sanitize(&c.detail, false)
            ))?;
        }
    }
    match failed {
        0 => Ok(()),
        1 => anyhow::bail!("1 check failed"),
        n => anyhow::bail!("{n} checks failed"),
    }
}

/// Generated into memory first: `clap_complete` panics on a failed write.
pub fn completion(ctx: &Ctx, args: CompletionArgs) -> Result<()> {
    let mut script = Vec::new();
    clap_complete::generate(args.shell, &mut Cli::command(), "bonsai", &mut script);
    ctx.out.data_bytes(&script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_version_matches_the_contract_crate() {
        let contract = bonsai_contract::Contract::load_default();
        assert_eq!(contract_version(), contract.info_version());
    }
}
