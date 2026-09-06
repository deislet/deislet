//! One list envelope and one opaque-cursor contract for the Control API.

use deis_wire::CanonicalId;
use serde::{Deserialize, Serialize};

use crate::catalog::{CatalogPage, CatalogStore, PagePosition};

use super::error::{ApiError, ApiResult};

pub const DEFAULT_LIST_LIMIT: usize = 100;
pub const MAX_LIST_LIMIT: usize = 1_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

impl ListQuery {
    pub fn bounded_limit(&self) -> ApiResult<usize> {
        let limit = self.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        if !(1..=MAX_LIST_LIMIT).contains(&limit) {
            return Err(ApiError::BadRequest(format!(
                "limit must be between 1 and {MAX_LIST_LIMIT}"
            )));
        }
        Ok(limit)
    }

    pub async fn position(
        &self,
        store: &CatalogStore,
        collection: &str,
        query_binding: &str,
    ) -> ApiResult<Option<PagePosition>> {
        match self.cursor.as_deref() {
            Some(cursor) => Ok(Some(
                store
                    .resolve_page_cursor(cursor, collection, query_binding)
                    .await?,
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
    store: &CatalogStore,
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
            store
                .issue_page_cursor(collection, query_binding, position_of(last))
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
