//! `bonsai url`: the connection URL, password included, and nothing else. The only
//! command that prints a secret.

use anyhow::Result;
use bonsai_core::{CoreError, connection_url};
use secrecy::{ExposeSecret, SecretString};

use super::Ctx;
use super::cluster::resolve;
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
    let a = ctx.authed().await?;
    let cluster = resolve(ctx, &a.client, &args.cluster.cluster).await?.value;
    let credential = args.credential.as_deref();
    let url = ctx
        .abortable(async { Ok(connection_url(&a.client, &cluster.id, credential).await?) })
        .await?;
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
}
