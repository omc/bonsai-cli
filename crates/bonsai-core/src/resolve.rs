//! Naming a cluster by ID or name.

use bonsai_api::{ApiError, Client, Cluster, ClusterQuery, Response, collect_pages};

use crate::CoreError;

/// The cluster `arg` names: its ID first, then an exact, case-insensitive name.
///
/// `GET /clusters/{arg}`; on a 404 (or an `arg` that can't be an ID, such as `..`),
/// `GET /clusters?name={arg}` across every page. One match is fetched by ID, so the
/// response is the cluster's own; several are [`CoreError::Ambiguous`], none
/// [`CoreError::ClusterNotFound`].
pub async fn resolve_cluster(api: &Client, arg: &str) -> Result<Response<Cluster>, CoreError> {
    match api.get_cluster(arg).await {
        Ok(found) => return Ok(found),
        Err(e) if e.status() == Some(404) || matches!(e, ApiError::InvalidPath(_)) => {}
        Err(e) => return Err(e.into()),
    }
    let query = ClusterQuery {
        name: Some(arg.to_owned()),
        ..ClusterQuery::default()
    };
    let (found, _) = collect_pages(None, |cursor| {
        let query = &query;
        async move { api.list_clusters(query, cursor.as_deref()).await }
    })
    .await?;
    let wanted = arg.to_lowercase();
    let mut ids: Vec<String> = found
        .into_iter()
        .filter(|c| c.name.to_lowercase() == wanted)
        .map(|c| c.id)
        .collect();
    match ids.len() {
        0 => Err(CoreError::ClusterNotFound(arg.to_owned())),
        1 => Ok(api.get_cluster(&ids.remove(0)).await?),
        _ => Err(CoreError::Ambiguous {
            name: arg.to_owned(),
            ids,
        }),
    }
}
