//! `bonsai cluster url`: the connection URL, password included, and nothing else
//! (`{"url": …}` under `--json`). The only command that prints a secret; with
//! `--write` it sets it in an env file instead.

use anyhow::Result;
use bonsai_api::Client;
use bonsai_core::{CoreError, connection_url, on_cluster};
use secrecy::{ExposeSecret, SecretString};

use super::cluster::resolve;
use super::{Ctx, chain, env_file};
use crate::cli::UrlArgs;

pub async fn url(ctx: &Ctx, args: UrlArgs) -> Result<()> {
    if let Some(name) = &args.env
        && !is_env_name(name)
    {
        return Err(CoreError::Usage(format!(
            "--env {name:?} isn't a variable name (letters, digits and _, not starting with a digit)"
        ))
        .into());
    }
    // Checked before the URL is fetched, so a refusal never holds the secret.
    let target = match &args.write {
        Some(path) => {
            let root = std::env::current_dir()?;
            let target = env_file::Target::check(&root, path)
                .map_err(|e| CoreError::Usage(format!("--write: {}", chain(e))))?;
            Some((root, target))
        }
        None => None,
    };
    let a = ctx.authed().await?;
    let url = fetch(
        ctx,
        &a.client,
        &args.cluster.cluster,
        args.credential.as_deref(),
    )
    .await?;
    let Some((root, target)) = target else {
        if ctx.out.is_json() {
            return ctx.out.json(&printed(args.env.as_deref(), &url));
        }
        // Written raw: the URL is the data, and sanitizing it would change it.
        return ctx.out.data_line(&line(args.env.as_deref(), &url));
    };
    let var = args.env.as_deref().unwrap_or(env_file::DEFAULT_VAR);
    let written = target.write(&root, var, &url)?;
    if ctx.out.is_json() {
        return ctx.out.json(&serde_json::json!({
            "path": written.path,
            "variable": var,
            "url": written.redacted,
            "replaced": written.replaced,
        }));
    }
    let how = if written.replaced {
        "Replaced"
    } else {
        "Added"
    };
    ctx.out.success(&format!("{how} {var} in {}", written.path));
    Ok(())
}

/// The connection URL of `credential` on the cluster `arg` names, or of the
/// cluster's default credential. `cluster url` prints it; `cluster request` sends
/// with it.
pub(super) async fn fetch(
    ctx: &Ctx,
    client: &Client,
    arg: &str,
    credential: Option<&str>,
) -> Result<SecretString> {
    match credential {
        // The cluster's own request first; its 404 can only mean no such cluster.
        None => {
            ctx.abortable(async {
                Ok(on_cluster(client, arg, |id| async move {
                    connection_url(client, &id, None).await
                })
                .await?)
            })
            .await
        }
        // A 404 here could be the credential's, so the cluster is found first.
        Some(credential) => {
            let cluster = resolve(ctx, client, arg).await?.value;
            ctx.abortable(async {
                Ok(connection_url(client, &cluster.id, Some(credential)).await?)
            })
            .await
        }
    }
}

/// `url`, or `NAME=url` with `--env NAME`.
fn line(env: Option<&str>, url: &SecretString) -> String {
    match env {
        Some(name) => format!("{name}={}", url.expose_secret()),
        None => url.expose_secret().to_owned(),
    }
}

/// `--json`: `{"url": …}`, with `"variable"` under `--env NAME`.
fn printed(env: Option<&str>, url: &SecretString) -> serde_json::Value {
    let mut v = serde_json::json!({ "url": url.expose_secret() });
    if let Some(name) = env {
        v["variable"] = name.into();
    }
    v
}

/// A POSIX shell variable name: `[A-Za-z_][A-Za-z0-9_]*`.
pub(super) fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names() {
        for ok in ["BONSAI_URL", "_x", "a1"] {
            assert!(is_env_name(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A B", "A=B", "É"] {
            assert!(!is_env_name(bad), "{bad}");
        }
    }

    #[test]
    fn env_prefixes_the_url() {
        let url = SecretString::from("https://u:p@h".to_owned());
        assert_eq!(line(None, &url), "https://u:p@h");
        assert_eq!(line(Some("ES_URL"), &url), "ES_URL=https://u:p@h");
    }

    #[test]
    fn json_is_an_object_with_the_url() {
        let url = SecretString::from("https://u:p@h".to_owned());
        assert_eq!(
            printed(None, &url),
            serde_json::json!({ "url": "https://u:p@h" })
        );
        assert_eq!(
            printed(Some("ES_URL"), &url),
            serde_json::json!({ "url": "https://u:p@h", "variable": "ES_URL" })
        );
    }
}
