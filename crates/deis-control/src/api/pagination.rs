//! One list envelope and one opaque-cursor contract for the Control API.

use deis_wire::CanonicalId;
use serde::{Deserialize, Serialize};

use crate::catalog::{CatalogPage, PageCursorStore, PagePosition};

use super::error::{ApiError, ApiResult};

pub const DEFAULT_LIST_LIMIT: usize = 100;
pub const MAX_LIST_LIMIT: usize = 1_000;
/// The furthest an offset-paged diagnostic route may skip. Offset paging has
/// no cursor to hold a position, so the only protection against a query that
/// walks the whole table is a ceiling on the offset itself.
pub const MAX_LIST_OFFSET: usize = 1_000_000;

/// Bound a raw `limit` query parameter: absent means [`DEFAULT_LIST_LIMIT`],
/// present must be in `1..=MAX_LIST_LIMIT`.
///
/// [`ListQuery::bounded_limit`] is this rule for the unified list envelope.
/// The diagnostic routes that still answer in their own shape - heartbeat
/// history, configuration history, the event log - keep that shape but share
/// this bound, so one uncapped `limit` cannot hand SQLite an unbounded read
/// and clients see the same rule and the same message everywhere.
pub fn bounded_limit(limit: Option<usize>) -> ApiResult<usize> {
    let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {MAX_LIST_LIMIT}"
        )));
    }
    Ok(limit)
}

/// Bound a raw `offset` query parameter: absent means 0, present must be in
/// `0..=MAX_LIST_OFFSET`. Negative values never reach here - they fail to
/// deserialize into `usize` and are refused as a malformed query.
pub fn bounded_offset(offset: Option<usize>) -> ApiResult<usize> {
    let offset = offset.unwrap_or(0);
    if offset > MAX_LIST_OFFSET {
        return Err(ApiError::BadRequest(format!(
            "offset must be between 0 and {MAX_LIST_OFFSET}"
        )));
    }
    Ok(offset)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

impl ListQuery {
    pub fn bounded_limit(&self) -> ApiResult<usize> {
        bounded_limit(self.limit)
    }

    pub async fn position(
        &self,
        cursors: &PageCursorStore,
        collection: &str,
        query_binding: &str,
    ) -> ApiResult<Option<PagePosition>> {
        match self.cursor.as_deref() {
            Some(cursor) => Ok(Some(
                cursors.resolve(cursor, collection, query_binding).await?,
            )),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageMetadata {
    pub limit: usize,
    pub count: usize,
    pub total: usize,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlListPage<T> {
    pub schema_version: u8,
    pub items: Vec<T>,
    pub page: PageMetadata,
}

pub async fn list_page<T, F>(
    cursors: &PageCursorStore,
    collection: &str,
    query_binding: &str,
    limit: usize,
    page: CatalogPage<T>,
    position_of: F,
) -> ApiResult<ControlListPage<T>>
where
    F: FnOnce(&T) -> PagePosition,
{
    let next_cursor = if page.has_more {
        let last = page.items.last().ok_or_else(|| {
            ApiError::InternalError(
                "paginated query reported more rows without returning a cursor position"
                    .to_string(),
            )
        })?;
        Some(
            cursors
                .issue(collection, query_binding, position_of(last))
                .await?,
        )
    } else {
        None
    };
    let count = page.items.len();
    Ok(ControlListPage {
        schema_version: 1,
        items: page.items,
        page: PageMetadata {
            limit,
            count,
            total: page.total,
            next_cursor,
        },
    })
}

pub fn id_scope(scope: &str, id: CanonicalId) -> String {
    format!("scope={scope}:{id}")
}

/// The cursor scope of a list that hangs off `app + environment`. Both ids
/// are canonical, so renaming neither the application nor the environment
/// (if that ever becomes possible) can make an old cursor resolve elsewhere.
pub fn environment_scope(application: CanonicalId, environment: CanonicalId) -> String {
    format!("scope=application:{application}/environment:{environment}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_limit_is_the_default_and_the_ceiling_is_inclusive() {
        assert_eq!(bounded_limit(None).unwrap(), DEFAULT_LIST_LIMIT);
        assert_eq!(bounded_limit(Some(1)).unwrap(), 1);
        assert_eq!(bounded_limit(Some(MAX_LIST_LIMIT)).unwrap(), MAX_LIST_LIMIT);
    }

    #[test]
    fn a_limit_outside_the_range_is_a_bad_request() {
        for limit in [0, MAX_LIST_LIMIT + 1, usize::MAX] {
            match bounded_limit(Some(limit)) {
                Err(ApiError::BadRequest(message)) => {
                    assert_eq!(message, "limit must be between 1 and 1000");
                }
                other => panic!("limit {limit} should be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_offset_past_the_ceiling_is_a_bad_request() {
        assert_eq!(bounded_offset(None).unwrap(), 0);
        assert_eq!(
            bounded_offset(Some(MAX_LIST_OFFSET)).unwrap(),
            MAX_LIST_OFFSET
        );
        match bounded_offset(Some(MAX_LIST_OFFSET + 1)) {
            Err(ApiError::BadRequest(message)) => {
                assert_eq!(message, "offset must be between 0 and 1000000");
            }
            other => panic!("an absurd offset should be refused, got {other:?}"),
        }
    }
}
