//! Following `next_cursor` across pages.

use crate::client::Response;
use crate::models::Page;
use crate::problem::ApiError;
use serde_json::Value;

/// Fetches pages until `next_cursor` is `null` or `limit` items are in hand.
///
/// `fetch` gets the cursor for the page it should fetch (`None` for the first).
/// Returns the typed items and, in the same order, each item as the server sent it.
pub async fn collect_pages<T, F, Fut>(
    limit: Option<usize>,
    mut fetch: F,
) -> Result<(Vec<T>, Vec<Value>), ApiError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<Response<Page<T>>, ApiError>>,
{
    let mut items = Vec::new();
    let mut raw = Vec::new();
    let mut cursor = None;
    // Every cursor sent so far: A -> B -> A is as endless as A -> A.
    let mut seen = std::collections::HashSet::new();
    loop {
        if limit.is_some_and(|l| items.len() >= l) {
            break;
        }
        let page = fetch(cursor.take()).await?;
        let raw_data = match page.raw.get("data") {
            Some(Value::Array(data)) if data.len() == page.value.data.len() => data.clone(),
            _ => return Err(ApiError::Decode("page `data` isn't an array".into())),
        };
        items.extend(page.value.data);
        raw.extend(raw_data);
        match page.value.next_cursor {
            // A cursor already sent would fetch the same pages forever.
            Some(next) if !seen.insert(next.clone()) => {
                return Err(ApiError::Decode(format!(
                    "the server returned cursor {next:?} again"
                )));
            }
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    if let Some(l) = limit {
        items.truncate(l);
        raw.truncate(l);
    }
    Ok((items, raw))
}
