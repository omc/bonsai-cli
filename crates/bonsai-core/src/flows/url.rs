//! `bonsai url`: the connection URL, with its password.

use bonsai_api::{AccessType, Client, collect_pages};
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
            let mut rw: Vec<String> = all
                .into_iter()
                .filter(|c| c.access_type == AccessType::Rw)
                .map(|c| c.id)
                .collect();
            match rw.len() {
                0 => return Err(CoreError::NoCredential(cluster_id.to_owned())),
                1 => rw.remove(0),
                _ => {
                    return Err(CoreError::SeveralCredentials {
                        cluster: cluster_id.to_owned(),
                        ids: rw,
                    });
                }
            }
        }
    };
    let cred = api.get_cluster_credential(cluster_id, &id).await?;
    Ok(SecretString::from(cred.value.url))
}
