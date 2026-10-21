//! `bonsai url`: the connection URL, password included, and nothing else. The only
//! command that prints a secret.

use anyhow::Result;
use bonsai_api::{Client, collect_pages};
use bonsai_core::{CoreError, connection_url};
use secrecy::{ExposeSecret, SecretString};

use super::Ctx;
use super::cluster::{prompt_error, resolve};
use crate::cli::UrlArgs;
use crate::sanitize::sanitize;

pub async fn url(ctx: &Ctx, args: UrlArgs) -> Result<()> {
    if let Some(name) = &args.env
        && !is_env_name(name)
    {
        return Err(CoreError::Usage(format!(
            "--env {name:?} isn't a variable name (letters, digits and _, not starting with a digit)"
        ))
        .into());
    }
    let a = ctx.authed().await?;
    let cluster = resolve(ctx, &a.client, &args.cluster.cluster).await?.value;
    let credential = args.credential.as_deref();
    let found = ctx
        .abortable(async { Ok(connection_url(&a.client, &cluster.id, credential).await) })
        .await?;
    let url = match found {
        Err(CoreError::SeveralCredentials { ids, .. }) if ctx.interactive => {
            let picked = pick_credential(ctx, &a.client, &cluster.id, &ids).await?;
            ctx.abortable(async {
                Ok(connection_url(&a.client, &cluster.id, Some(&picked)).await?)
            })
            .await?
        }
        other => other?,
    };
    // Written raw: the URL is the data, and sanitizing it would change it.
    ctx.out.data_line(&line(args.env.as_deref(), &url))
}

/// `url`, or `NAME=url` with `--env NAME`.
fn line(env: Option<&str>, url: &SecretString) -> String {
    match env {
        Some(name) => format!("{name}={}", url.expose_secret()),
        None => url.expose_secret().to_owned(),
    }
}

/// A POSIX shell variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Asks which of the cluster's credentials `ids` to use, showing name, access type
/// and ID.
async fn pick_credential(
    ctx: &Ctx,
    client: &Client,
    cluster_id: &str,
    ids: &[String],
) -> Result<String> {
    let (all, _) = ctx
        .abortable(async {
            Ok(collect_pages(None, |cursor| async move {
                client
                    .list_cluster_credentials(cluster_id, cursor.as_deref())
                    .await
            })
            .await?)
        })
        .await?;
    let offered: Vec<_> = all.into_iter().filter(|c| ids.contains(&c.id)).collect();
    let labels: Vec<String> = offered
        .iter()
        .map(|c| {
            let name = c.name.as_deref().unwrap_or("(no name)");
            sanitize(&format!("{name} ({}, {})", c.access_type, c.id), false).into_owned()
        })
        .collect();
    let picked = inquire::Select::new("Credential", labels)
        .raw_prompt()
        .map_err(prompt_error)?;
    Ok(offered[picked.index].id.clone())
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
}
