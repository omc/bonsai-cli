//! Naming a cluster by ID or name.

use bonsai_api::{ApiError, Client, Cluster, ClusterQuery, Response, collect_pages};

use crate::CoreError;

/// The cluster `arg` names: its ID first, then an exact, case-insensitive name.
///
/// `GET /clusters/{arg}`; on a 404 (or an `arg` that can't be an ID, such as `..`),
/// `GET /clusters?name={arg}` across every page. One match is the answer as the list
/// gave it (a whole `Cluster`, so it isn't fetched again; its response headers are
/// the list's, and nothing reads them); several are [`CoreError::Ambiguous`], none
/// [`CoreError::ClusterNotFound`].
pub async fn resolve_cluster(api: &Client, arg: &str) -> Result<Response<Cluster>, CoreError> {
    match api.get_cluster(arg).await {
        Ok(found) => Ok(found),
        Err(e) if not_an_id(&e) => {
            // The 404 just logged at -v is this lookup, not a failure.
            tracing::debug!("{arg:?} isn't a cluster ID; looking it up by name");
            resolve_by_name(api, arg).await
        }
        Err(e) => Err(e.into()),
    }
}

/// `call` on the cluster `arg` names, trying `arg` as its ID first, so an ID costs
/// only `call`'s own requests. When that's a 404 (or `arg` can't be an ID), `arg` is
/// looked up by name as [`resolve_cluster`] does and `call` runs again with the ID.
///
/// Only for calls whose 404 can only mean "no such cluster": a call that names
/// something else as well, such as a credential, would read its own 404 as a name.
pub async fn on_cluster<T, F, Fut>(api: &Client, arg: &str, call: F) -> Result<T, CoreError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<T, CoreError>>,
{
    match call(arg.to_owned()).await {
        Err(CoreError::Api(e)) if not_an_id(&e) => {
            tracing::debug!("{arg:?} isn't a cluster ID; looking it up by name");
            let cluster = resolve_by_name(api, arg).await?;
            call(cluster.value.id).await
        }
        other => other,
    }
}

/// A 404, or an argument that can't be put in a path: not an ID, maybe a name.
fn not_an_id(e: &ApiError) -> bool {
    e.status() == Some(404) || matches!(e, ApiError::InvalidPath(_))
}

/// The cluster named `arg`: `GET /clusters?name={arg}`, matched exactly and
/// case-insensitively.
async fn resolve_by_name(api: &Client, arg: &str) -> Result<Response<Cluster>, CoreError> {
    let query = ClusterQuery {
        name: Some(arg.to_owned()),
        ..ClusterQuery::default()
    };
    let (found, raw) = collect_pages(None, |cursor| {
        let query = &query;
        async move { api.list_clusters(query, cursor.as_deref()).await }
    })
    .await?;
    let wanted = arg.to_lowercase();
    let mut matches: Vec<_> = found
        .into_iter()
        .zip(raw)
        .filter(|(c, _)| c.name.to_lowercase() == wanted)
        .collect();
    match matches.len() {
        0 => Err(CoreError::ClusterNotFound(arg.to_owned())),
        1 => {
            let (value, raw) = matches.remove(0);
            Ok(Response {
                value,
                raw,
                status: 200,
                retry_after: None,
                location: None,
                idempotent_replayed: false,
                date: None,
            })
        }
        _ => Err(CoreError::Ambiguous {
            name: arg.to_owned(),
            ids: matches.into_iter().map(|(c, _)| c.id).collect(),
        }),
    }
}
