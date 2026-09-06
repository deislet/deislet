//! One list envelope and one opaque-cursor contract for the Control API.

use std::fmt::Display;
use std::str::FromStr;

use axum::http::StatusCode;
use deis_wire::CanonicalId;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::catalog::{CatalogPage, PageCursorStore, PagePosition};
use crate::database::KeysetAfter;

use super::error::{ApiError, ApiResult, codes};

pub const DEFAULT_LIST_LIMIT: usize = 100;
pub const MAX_LIST_LIMIT: usize = 1_000;

/// Bound a raw `limit` query parameter: absent means [`DEFAULT_LIST_LIMIT`],
/// present must be in `1..=MAX_LIST_LIMIT`.
///
/// [`ListQuery::bounded_limit`] is this rule for the unified list envelope,
/// and every list route answers in that envelope, so one uncapped `limit`
/// cannot hand SQLite an unbounded read and clients see the same rule and
/// the same message everywhere.
pub fn bounded_limit(limit: Option<usize>) -> ApiResult<usize> {
    let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        // The sentence is the one every list route has always answered
        // with; the code and the target are what the structured surfaces
        // add, so a client knows it was `limit` without parsing it.
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            codes::LIMIT_OUT_OF_RANGE,
            format!("limit must be between 1 and {MAX_LIST_LIMIT}"),
        )
        .with_target("query", Some("limit".to_string())));
    }
    Ok(limit)
}

/// The two query parameters every list route understands. A route that
/// filters adds its own fields to a query type of its own and documents
/// them there; these two keep their meaning everywhere.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Page size, `1..=1000`; absent means 100.
    #[param(minimum = 1, maximum = 1000)]
    pub limit: Option<usize>,
    /// The `page.nextCursor` of the previous answer. Opaque, bound to the
    /// collection and the filters it was issued for, and refused with 400
    /// `cursor-invalid` anywhere else.
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

/// How much of the collection one answer covers; see
/// `contracts/v1/control-list-page.schema.json`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PageMetadata {
    /// The page size that was applied.
    pub limit: usize,
    /// How many items this answer carries.
    pub count: usize,
    /// How many items match the query in total, read in the same
    /// transaction as the items.
    pub total: usize,
    /// Where the next page starts; `null` on the last page.
    #[schema(required)]
    pub next_cursor: Option<String>,
}

/// The one list envelope of the Control API. Every list route answers in it,
/// whatever it lists.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControlListPage<T> {
    /// Always `1` for this document.
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

/// The same page with every item converted: the store's row type becomes
/// the route's item type without touching `total` or `has_more`.
pub fn map_page<T, U>(page: CatalogPage<T>, item: impl FnMut(T) -> U) -> CatalogPage<U> {
    CatalogPage {
        items: page.items.into_iter().map(item).collect(),
        total: page.total,
        has_more: page.has_more,
    }
}

pub fn id_scope(scope: &str, id: CanonicalId) -> String {
    format!("scope={scope}:{id}")
}

/// A text value inside a cursor binding - a scope id that is not canonical,
/// or a filter - written as a JSON string literal so a value containing the
/// binding's own separators cannot read as a different scope or filter set.
pub fn binding_text(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// An optional filter inside a cursor binding: its value quoted by
/// [`binding_text`], or `all` when the filter is absent.
pub fn binding_filter(value: Option<&str>) -> String {
    value.map_or_else(|| "all".to_string(), binding_text)
}

/// The cursor scope of a list that hangs off one node of the control
/// channel: the node's id as the node announced it. Node ids are not
/// canonical ids, so the value is quoted.
pub fn node_scope(node_id: &str) -> String {
    format!("scope=node:{}", binding_text(node_id))
}

/// The cursor position of a row in a diagnostic history sorted on an
/// RFC 3339 text column, newest first, with the row id as the tie-breaker
/// (see [`KeysetAfter`]). The text goes in `secondary` verbatim, the id in
/// `identity`; `sort_value` has nothing to carry and is zero.
pub fn text_keyset_position(sort_text: String, id: impl Display) -> PagePosition {
    PagePosition {
        sort_value: 0,
        identity: id.to_string(),
        secondary: Some(sort_text),
    }
}

/// The inverse of [`text_keyset_position`]: where a resolved cursor says a
/// diagnostic history resumes. A stored position that does not carry the
/// sort text, or whose identity is not an id of the table, was not issued
/// for this history and is refused as a bad cursor.
pub fn text_keyset_after<Id: FromStr>(
    position: Option<PagePosition>,
    history: &str,
) -> ApiResult<Option<KeysetAfter<Id>>> {
    let Some(position) = position else {
        return Ok(None);
    };
    // Same class as a cursor the store refused, and the same code: this
    // one resolved, but what it says cannot address a row of this list, so
    // it was issued for another one. The sentence is unchanged.
    let malformed = || {
        ApiError::coded(
            StatusCode::BAD_REQUEST,
            codes::CURSOR_INVALID,
            format!("{history} cursor position is malformed"),
        )
        .with_target("query", Some("cursor".to_string()))
    };
    let sort_text = position.secondary.ok_or_else(malformed)?;
    let id = position.identity.parse::<Id>().map_err(|_| malformed())?;
    Ok(Some(KeysetAfter { sort_text, id }))
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

    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    /// One GET through the whole router, as a caller on the machine the
    /// control plane runs on - the shape `api::tests` builds, so these two
    /// refusals are asserted on the same mounts a client meets.
    async fn get(uri: &str) -> (StatusCode, Value) {
        let (router, _dir) = crate::api::tests::full_router().await;
        let mut request = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// What a client of the structured surfaces reads when it asks for a
    /// page it cannot have: the code and the parameter, on both refusals -
    /// while the flat tree keeps answering the one string it always did.
    #[tokio::test]
    async fn the_v1_surface_names_the_page_parameter_it_refused() {
        let (status, body) = get("/api/v1/apps?limit=1001").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], codes::LIMIT_OUT_OF_RANGE);
        assert_eq!(body["error"]["message"], "limit must be between 1 and 1000");
        assert_eq!(body["error"]["target"]["resource"], "query");
        assert_eq!(body["error"]["target"]["field"], "limit");
        assert_eq!(body["error"]["retryable"], false);

        let (status, body) = get("/api/v1/apps?cursor=not-a-cursor").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], codes::CURSOR_INVALID);
        assert_eq!(
            body["error"]["message"],
            crate::catalog::INVALID_CURSOR_MESSAGE
        );
        assert_eq!(body["error"]["target"]["field"], "cursor");

        // The legacy tree is untouched: one key, the same sentence.
        let (status, body) = get("/api/apps?limit=1001").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            serde_json::json!({"error": "limit must be between 1 and 1000"})
        );
        let (status, body) = get("/api/apps?cursor=not-a-cursor").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            serde_json::json!({ "error": crate::catalog::INVALID_CURSOR_MESSAGE })
        );
    }

    #[test]
    fn an_absent_limit_is_the_default_and_the_ceiling_is_inclusive() {
        assert_eq!(bounded_limit(None).unwrap(), DEFAULT_LIST_LIMIT);
        assert_eq!(bounded_limit(Some(1)).unwrap(), 1);
        assert_eq!(bounded_limit(Some(MAX_LIST_LIMIT)).unwrap(), MAX_LIST_LIMIT);
    }

    /// The sentence is frozen (the legacy tree answers it byte for byte);
    /// the code and the target are what a structured client reads.
    #[test]
    fn a_limit_outside_the_range_is_refused_as_limit_out_of_range() {
        for limit in [0, MAX_LIST_LIMIT + 1, usize::MAX] {
            let error = bounded_limit(Some(limit)).expect_err("limit {limit} must be refused");
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert_eq!(error.code(), codes::LIMIT_OUT_OF_RANGE);
            let detail = error.into_detail();
            assert_eq!(detail.legacy_message, "limit must be between 1 and 1000");
            assert_eq!(detail.message, "limit must be between 1 and 1000");
            let target = detail.target.expect("the refusal names the parameter");
            assert_eq!(target.resource, "query");
            assert_eq!(target.field.as_deref(), Some("limit"));
        }
    }

    #[test]
    fn a_binding_quotes_what_a_client_chose_and_names_what_it_left_out() {
        assert_eq!(binding_text("node-a"), "\"node-a\"");
        assert_eq!(
            binding_text("a\";sort=id-asc"),
            "\"a\\\";sort=id-asc\"",
            "the separators of a binding stay inside one quoted value"
        );
        assert_eq!(binding_filter(None), "all");
        assert_eq!(binding_filter(Some("Error")), "\"Error\"");
        assert_eq!(node_scope("n;1"), "scope=node:\"n;1\"");
    }

    #[test]
    fn a_text_keyset_position_round_trips_and_a_foreign_one_is_refused() {
        let position = text_keyset_position("2026-09-06T10:00:00.5+00:00".to_string(), 42);
        assert_eq!(position.sort_value, 0);
        assert_eq!(position.identity, "42");
        assert_eq!(
            position.secondary.as_deref(),
            Some("2026-09-06T10:00:00.5+00:00")
        );
        let after = text_keyset_after::<i64>(Some(position.clone()), "events")
            .unwrap()
            .unwrap();
        assert_eq!(after.sort_text, "2026-09-06T10:00:00.5+00:00");
        assert_eq!(after.id, 42);
        assert!(text_keyset_after::<i64>(None, "events").unwrap().is_none());

        // A command uuid is an identity of the command history and of no
        // integer-keyed one.
        let command = text_keyset_position("t".to_string(), "0f1e2d3c");
        assert_eq!(
            text_keyset_after::<String>(Some(command.clone()), "commands")
                .unwrap()
                .unwrap()
                .id,
            "0f1e2d3c"
        );
        let error = text_keyset_after::<i64>(Some(command), "events")
            .expect_err("a uuid is not an event id");
        assert_eq!(error.code(), codes::CURSOR_INVALID);
        assert_eq!(
            error.into_detail().legacy_message,
            "events cursor position is malformed"
        );
        // A position without the sort text was issued for an integer-sorted
        // list and cannot say where a text-sorted one resumes.
        let integer_sorted = PagePosition {
            sort_value: 7,
            identity: "42".to_string(),
            secondary: None,
        };
        assert_eq!(
            text_keyset_after::<i64>(Some(integer_sorted), "events")
                .expect_err("a position without the sort text cannot resume this list")
                .code(),
            codes::CURSOR_INVALID
        );
    }
}
