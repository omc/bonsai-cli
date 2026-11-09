//! `bonsai cluster url`: the connection URL, with its password.

use bonsai_api::{Client, collect_pages};
use secrecy::SecretString;

use crate::CoreError;

/// The `https://user:pass@host` URL of `credential`, or of the cluster's only `rw`
/// credential when none is named.
pub async fn connection_url(
    api: &Client,
    cluster_id: &str,
    credential: Option<&str>,
) -> Result<SecretString, CoreError> {
    let id = match credential {
        Some(id) => id.to_owned(),
        None => {
            let (all, _) = collect_pages(None, |cursor| async move {
                api.list_cluster_credentials(cluster_id, cursor.as_deref())
                    .await
            })
            .await?;
            // The one the dashboard shows; a cluster has one while it has an rw
            // credential.
            all.into_iter()
                .find(|c| c.default)
                .map(|c| c.id)
                .ok_or_else(|| CoreError::NoCredential(cluster_id.to_owned()))?
        }
    };
    let cred = api.get_cluster_credential(cluster_id, &id).await?;
    Ok(SecretString::from(cred.value.url))
}
