//! Query-plan evidence for every keyset page query in Control.
//!
//! Each public list pages by a versioned composite sort (docs/adr/016, list
//! and query section) and the schema carries one index per sort so a page
//! deep in a large collection costs one index seek, not a sort of the whole
//! scope. Nothing enforced that pairing until now: an index can be dropped,
//! renamed or written with the wrong column order and every test still
//! passes, because a correct answer arrives either way - only slower, and
//! only on a database large enough that no unit test notices.
//!
//! These tests ask SQLite itself. Every statement explained here is
//! rendered by the same builder the store prepares its page from, so the
//! evidence cannot drift away from the query it is evidence for: change the
//! statement and this file explains the changed one. `EXPLAIN QUERY PLAN`
//! then has to name the intended index and never fall back to a temporary
//! sort or a bare table scan.
//!
//! Every list is explained twice, because a keyset page has two statements
//! and only the second one carries the cost. A first page has no cursor
//! predicate at all. A resumed page adds one, and the plan then has to show
//! that predicate *inside the index seek* - `(env_id=? AND created_at<?)`
//! and not `(env_id=?)` with the cursor left to a row-by-row comparison.
//! The difference never shows in an answer, only in how many index entries
//! a deep page steps over: with the seek, as many as the page returns;
//! without it, every row of the scope that precedes the cursor.
//!
//! Only the items query is checked. The `COUNT(*)` that accompanies it may
//! legitimately scan the scope.

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use deis_provider_driver::{
    ArtifactVersionRef, CredentialMaterial, ProviderDriverManifest, ProviderNeutralPlan,
    Sha256Digest, SupportedStandard, ToolDigest, ValidatedProviderDriverManifest,
};
use deis_wire::CanonicalId;
use rusqlite::types::Value;
use rusqlite::{Connection, params};

use super::pagination::{
    KeysetBound, KeysetDirection, KeysetResume, PageCursorStore, PagePosition,
};
use super::provider_credential::{credential_audit_events_page_sql, credential_bindings_page_sql};
use super::provider_driver_operation::{
    environment_deployments_page_sql, installations_page_sql, targets_by_installation_page_sql,
    targets_page_sql,
};
use super::provider_driver_release::{release_audit_events_page_sql, releases_page_sql};
use super::provider_lifecycle::lifecycle_audit_events_page_sql;
use super::provider_target_grant::{
    environment_page_sql, grant_audit_events_page_sql, history_page_sql,
};
use super::store::{
    apps_page_sql, canonical_artifacts_page_sql, deployments_page_sql, domain_grants_page_sql,
    environments_page_sql, retired_names_page_sql, routes_page_sql, secrets_page_sql,
    versions_page_sql,
};
use super::{
    CatalogProviderCredentialVault, CatalogStore, OperationRequestContext,
    ProviderCredentialBindingSpec, ProviderDeploymentSpec, ProviderInstallationSpec,
    ProviderLifecycleMutationContext, ProviderLifecycleResourceKind,
    ProviderTargetGrantMutationContext, ProviderTargetGrantSpec, ProviderTargetSpec, SecretCipher,
    SecretsBackend, now_secs,
};
use crate::audit::{AuditQuery, AuditRecord, AuditStore, Outcome, audit_page_sql};
use crate::database::{
    COMMAND_COLUMNS, CONFIG_HISTORY_COLUMNS, CommandEntry, CommandStatus, Database, DatabaseError,
    EVENT_COLUMNS, Event, EventSeverity, HEARTBEAT_COLUMNS, HeartbeatEntry, KeysetPageShape,
    keyset_page_sql,
};
use crate::identity::store::{
    TOKEN_COLUMNS, accounts_page_sql, tokens_by_account_page_sql, tokens_page_sql,
};
use crate::identity::{Authority, IdentityStore, Principal, PrincipalKind, Role};
use crate::message::{NodeInfo, NodeRole};
use crate::session::NodeSession;

/// What the plan has to show beyond reaching the collection through an
/// index.
#[derive(Clone, Copy)]
enum Seek {
    /// A first page. There is no position to resume from, so the index has
    /// only to supply the sort and whatever scope the request named.
    FirstPage,
    /// A resumed page: this sort column has to appear as a range constraint
    /// of the index seek itself, which is what makes the page cost the same
    /// however deep it is.
    Resumed(&'static str),
    /// A resumed page whose cursor SQLite can only compare row by row, with
    /// the reason. Always a property of the sort the list promises, never
    /// of the index under it.
    RowFiltered(&'static str),
}

/// One page statement, rendered by the builder the store itself prepares.
struct KeysetQuery {
    name: String,
    /// The table (or its alias in the query) whose rows are being paged.
    table: &'static str,
    sql: String,
    /// Any one of these must appear in the plan. Two names where the query
    /// is written so that either index answers it.
    indexes: &'static [&'static str],
    seek: Seek,
}

/// The two statements one list prepares: a first page and a resumed one.
struct PageShape {
    name: &'static str,
    table: &'static str,
    indexes: &'static [&'static str],
    seek: Seek,
    first: String,
    resumed: String,
}

impl PageShape {
    fn queries(self) -> [KeysetQuery; 2] {
        [
            KeysetQuery {
                name: format!("{} (first page)", self.name),
                table: self.table,
                sql: self.first,
                indexes: self.indexes,
                seek: Seek::FirstPage,
            },
            KeysetQuery {
                name: format!("{} (resumed)", self.name),
                table: self.table,
                sql: self.resumed,
                indexes: self.indexes,
                seek: self.seek,
            },
        ]
    }
}

fn shapes(shapes: Vec<PageShape>) -> Vec<KeysetQuery> {
    shapes
        .into_iter()
        .flat_map(|shape| shape.queries())
        .collect()
}

/// The position the resumed statements are rendered for. Which position it
/// is cannot matter: the plan is fixed when the statement is compiled, long
/// before a value is bound to it.
fn position() -> PagePosition {
    PagePosition {
        sort_value: 0,
        identity: String::new(),
        secondary: None,
    }
}

/// The first-page and resumed cursors of a list sorted by one value column
/// with an identity tie-breaker - the shape almost every list has.
fn value_and_identity(
    first_param: usize,
    direction: KeysetDirection,
    value_column: &'static str,
    identity_column: &'static str,
) -> (KeysetResume, KeysetResume) {
    (
        KeysetResume::value_and_identity(
            first_param,
            direction,
            value_column,
            identity_column,
            None,
        ),
        KeysetResume::value_and_identity(
            first_param,
            direction,
            value_column,
            identity_column,
            Some(position()),
        ),
    )
}

/// The same for a list whose sort column is already unique.
fn identity_only(
    first_param: usize,
    direction: KeysetDirection,
    column: &'static str,
) -> (KeysetResume, KeysetResume) {
    (
        KeysetResume::identity_only(first_param, direction, column, None),
        KeysetResume::identity_only(first_param, direction, column, Some(String::new())),
    )
}

/// One list's evidence: its two statements, rendered by `build` from the
/// two cursors `resumes` describes.
fn shape(
    name: &'static str,
    table: &'static str,
    indexes: &'static [&'static str],
    seek: Seek,
    resumes: (KeysetResume, KeysetResume),
    build: impl Fn(&KeysetResume) -> String,
) -> PageShape {
    PageShape {
        name,
        table,
        indexes,
        seek,
        first: build(&resumes.0),
        resumed: build(&resumes.1),
    }
}

/// The plan SQLite chose for `sql`, one line per step. The plan is fixed
/// when the statement is compiled, so the placeholders are bound to NULL:
/// a value could not change the answer, and a real cursor position would
/// only suggest otherwise.
fn explain(connection: &Connection, sql: &str) -> Vec<String> {
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap_or_else(|error| panic!("cannot prepare {sql}: {error}"));
    let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
    statement
        .query_map(rusqlite::params_from_iter(nulls), |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// The constraint list SQLite printed for the step that uses `index`, for
/// example `env_id=? AND created_at<?`. `None` when the step named no
/// constraints at all, which is a walk of the whole index.
fn seek_constraints(plan: &[String], index: &str) -> Option<String> {
    let line = plan.iter().find(|line| line.contains(index))?;
    let after = &line[line.find(index)? + index.len()..];
    let start = after.find('(')?;
    let end = after.rfind(')')?;
    Some(after[start + 1..end].to_owned())
}

fn assert_keyset_plan(query: &KeysetQuery, plan: &[String]) {
    let text = plan.join("\n");
    let index = query
        .indexes
        .iter()
        .find(|index| text.contains(**index))
        .unwrap_or_else(|| {
            panic!(
                "{}: plan does not use any of {:?}:\n{text}",
                query.name, query.indexes
            )
        });
    assert!(
        !text.contains("TEMP B-TREE"),
        "{}: plan sorts with a temporary b-tree instead of walking the index:\n{text}",
        query.name
    );
    for line in plan {
        let bare_scan = line
            .strip_prefix("SCAN ")
            .is_some_and(|rest| rest.trim() == query.table || rest.starts_with("TABLE "));
        assert!(
            !bare_scan,
            "{}: plan scans {} without an index:\n{text}",
            query.name, query.table
        );
    }
    if let Seek::RowFiltered(reason) = query.seek {
        // The cursor cannot be a range constraint here, but the scope it
        // pages within still has to be one: without that the page would
        // read the whole collection rather than one environment's slice of
        // it.
        let constraints = seek_constraints(plan, index).unwrap_or_default();
        assert!(
            !constraints.is_empty(),
            "{}: {reason}, so the cursor is compared row by row - and the plan does not seek \
             even the scope through {index}, which leaves the page reading the whole \
             collection:\n{text}",
            query.name
        );
    }
    if let Seek::Resumed(column) = query.seek {
        // The statement may qualify the column with the table's alias; the
        // plan never does.
        let column = column.rsplit('.').next().unwrap();
        let constraints = seek_constraints(plan, index).unwrap_or_else(|| {
            panic!(
                "{}: the step on {index} names no constraint at all, so the cursor is a \
                 row-by-row comparison and the page walks the scope:\n{text}",
                query.name
            )
        });
        // Two spellings mean the same seek: `created_at<?` for a plain
        // comparison, and `(pattern,app_id,environment)>(?,?,?)` where the
        // sort is one row-value comparison over several columns.
        let seeked = constraints.contains(&format!("{column}<"))
            || constraints.contains(&format!("{column}>"))
            || (constraints.starts_with(&format!("({column},"))
                && (constraints.contains(")>(") || constraints.contains(")<(")));
        assert!(
            seeked,
            "{}: the cursor on {column} is not part of the seek through {index} \
             ({constraints}), so a deep page reads every row before it:\n{text}",
            query.name
        );
    }
}

/// Every page query of the catalog store (`store.rs`).
fn catalog_queries() -> Vec<KeysetQuery> {
    shapes(vec![
        shape(
            "list_apps_page",
            "apps",
            &["sqlite_autoindex_apps_1"],
            Seek::Resumed("id"),
            identity_only(1, KeysetDirection::Ascending, "id"),
            apps_page_sql,
        ),
        shape(
            "list_retired_names_page",
            "retired_app_names",
            &["sqlite_autoindex_retired_app_names_1"],
            Seek::Resumed("app_id"),
            identity_only(1, KeysetDirection::Ascending, "app_id"),
            retired_names_page_sql,
        ),
        shape(
            "list_canonical_artifacts_page",
            "canonical_artifacts",
            &["idx_canonical_artifacts_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(2, KeysetDirection::Descending, "created_at", "id"),
            canonical_artifacts_page_sql,
        ),
        shape(
            "list_versions_page",
            "versions",
            // `UNIQUE(app_id, number)` is the index that serves this page:
            // SQLite knows the pair is unique, so `id DESC` after `number
            // DESC` needs no sort. `idx_versions_app` would do as well.
            &["sqlite_autoindex_versions_2", "idx_versions_app"],
            Seek::Resumed("number"),
            value_and_identity(2, KeysetDirection::Descending, "number", "id"),
            versions_page_sql,
        ),
        shape(
            "list_environments_page",
            "environments",
            // `UNIQUE(app_id, name)`; the first autoindex is the primary key.
            &["sqlite_autoindex_environments_2"],
            Seek::Resumed("name"),
            identity_only(2, KeysetDirection::Ascending, "name"),
            environments_page_sql,
        ),
        shape(
            "list_deployments_page",
            "deployments",
            &["idx_deployments_env_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(2, KeysetDirection::Descending, "created_at", "canonical_id"),
            deployments_page_sql,
        ),
        shape(
            "list_routes_page",
            "routes",
            &["idx_routes_env_page_v1"],
            // Routes are the one list whose sort changes direction between
            // columns - `priority DESC, path_prefix ASC, id ASC` is the
            // order the router itself resolves a request in - so the resume
            // cannot be one range: `priority < ?` and `path_prefix > ?`
            // describe different sides of the same index. The seek is the
            // environment, and the cursor is compared over that
            // environment's routes, of which there are few by construction
            // (one per path prefix a deployment publishes).
            Seek::RowFiltered("the sort mixes DESC and ASC columns"),
            (
                KeysetResume::first_page(2),
                KeysetResume::after(
                    2,
                    vec![
                        KeysetBound::descending("priority", Value::from(0_i64)),
                        KeysetBound::ascending("path_prefix", Value::from(String::new())),
                        KeysetBound::ascending("id", Value::from(String::new())),
                    ],
                ),
            ),
            routes_page_sql,
        ),
        shape(
            "list_domain_grants_page",
            "domain_grants",
            &["sqlite_autoindex_domain_grants_1"],
            Seek::Resumed("pattern"),
            (
                KeysetResume::first_page(1),
                KeysetResume::after_row_value(
                    1,
                    vec![
                        KeysetBound::ascending("pattern", Value::from(String::new())),
                        KeysetBound::ascending("app_id", Value::from(String::new())),
                        KeysetBound::ascending("environment", Value::from(String::new())),
                    ],
                ),
            ),
            domain_grants_page_sql,
        ),
        shape(
            "list_secrets_page",
            "secret_heads",
            &["sqlite_autoindex_secret_heads_1"],
            Seek::Resumed("name"),
            identity_only(2, KeysetDirection::Ascending, "name"),
            secrets_page_sql,
        ),
    ])
}

/// The identity store's page queries (`identity/store.rs`).
fn identity_queries() -> Vec<KeysetQuery> {
    // The statement qualifies the token columns with the join's alias, the
    // way the store does before it renders either form.
    let columns = TOKEN_COLUMNS.replace(", ", ", t.");
    let unfiltered = columns.clone();
    shapes(vec![
        shape(
            "list_accounts_page",
            "accounts",
            &["idx_accounts_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(1, KeysetDirection::Ascending, "created_at", "name"),
            accounts_page_sql,
        ),
        // The token list prepares one statement per account filter so the
        // account is an equality the planner can seek.
        shape(
            "list_tokens_page unfiltered",
            "t",
            &["idx_api_tokens_page_v1"],
            Seek::Resumed("t.created_at"),
            value_and_identity(3, KeysetDirection::Ascending, "t.created_at", "t.id"),
            move |resume| tokens_page_sql(&unfiltered, resume),
        ),
        shape(
            "list_tokens_page by account",
            "t",
            &["idx_api_tokens_account_page_v1"],
            Seek::Resumed("t.created_at"),
            value_and_identity(4, KeysetDirection::Ascending, "t.created_at", "t.id"),
            move |resume| tokens_by_account_page_sql(&columns, resume),
        ),
    ])
}

/// `AuditStore::query_page` builds its statement from the filters present,
/// so the unfiltered log and each filter combination are distinct
/// statements with distinct intended indexes. The filter clauses come from
/// `AuditQuery` itself, so a query that learns a new filter cannot quietly
/// stop being explained here.
fn audit_queries() -> Vec<KeysetQuery> {
    fn case(name: &'static str, query: AuditQuery, indexes: &'static [&'static str]) -> PageShape {
        let (clauses, _) = query.clauses();
        let first_cursor_param = clauses.len() + 1;
        let build = |resume: &KeysetResume| audit_page_sql(&clauses, resume);
        PageShape {
            name,
            table: "audit_log",
            indexes,
            seek: Seek::Resumed("at"),
            first: build(&KeysetResume::first_page(first_cursor_param)),
            resumed: build(&KeysetResume::after(
                first_cursor_param,
                vec![
                    KeysetBound::descending("at", Value::from(0_i64)),
                    KeysetBound::descending("id", Value::from(0_i64)),
                ],
            )),
        }
    }
    let app = || Some("plan-evidence".to_owned());
    shapes(vec![
        case(
            "audit query_page unfiltered",
            AuditQuery::default(),
            &["idx_audit_page_v1"],
        ),
        case(
            "audit query_page by app",
            AuditQuery {
                app_id: app(),
                ..AuditQuery::default()
            },
            &["idx_audit_app_page_v1"],
        ),
        case(
            "audit query_page by app and environment",
            AuditQuery {
                app_id: app(),
                environment: Some("production".to_owned()),
                ..AuditQuery::default()
            },
            &["idx_audit_app_page_v1"],
        ),
        case(
            "audit query_page by account",
            AuditQuery {
                account: Some("alice".to_owned()),
                ..AuditQuery::default()
            },
            &["idx_audit_account_page_v1"],
        ),
        case(
            "audit query_page by outcome",
            AuditQuery {
                outcome: Some(Outcome::Refused),
                ..AuditQuery::default()
            },
            &["idx_audit_outcome_page_v1"],
        ),
        case(
            "audit query_page by action",
            AuditQuery {
                action: Some("deploy".to_owned()),
                ..AuditQuery::default()
            },
            &["idx_audit_page_v1"],
        ),
        case(
            "audit query_page since",
            AuditQuery {
                since: Some(0),
                ..AuditQuery::default()
            },
            &["idx_audit_page_v1"],
        ),
        case(
            "audit query_page since and until",
            AuditQuery {
                since: Some(0),
                until: Some(i64::MAX),
                ..AuditQuery::default()
            },
            &["idx_audit_page_v1"],
        ),
        case(
            "audit query_page by app since",
            AuditQuery {
                app_id: app(),
                since: Some(0),
                ..AuditQuery::default()
            },
            &["idx_audit_app_page_v1"],
        ),
    ])
}

/// The provider lists: installations, targets, deployments, grants,
/// credential bindings, driver releases and the three audit trails.
fn provider_queries() -> Vec<KeysetQuery> {
    shapes(vec![
        shape(
            "provider_installations_page",
            "provider_installations",
            &["idx_provider_installations_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(
                1,
                KeysetDirection::Ascending,
                "created_at",
                "installation_id",
            ),
            installations_page_sql,
        ),
        // The target list prepares one statement per installation filter so
        // the installation is an equality the planner can seek.
        shape(
            "provider_targets_page unfiltered",
            "provider_targets",
            &["idx_provider_targets_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(1, KeysetDirection::Ascending, "created_at", "target_id"),
            targets_page_sql,
        ),
        shape(
            "provider_targets_page by installation",
            "provider_targets",
            &["idx_provider_targets_installation_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(2, KeysetDirection::Ascending, "created_at", "target_id"),
            targets_by_installation_page_sql,
        ),
        shape(
            "provider_deployments_for_environment_page",
            "provider_deployment_workflows",
            &["idx_provider_deployment_environment_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(3, KeysetDirection::Descending, "created_at", "operation_id"),
            environment_deployments_page_sql,
        ),
        // The per-environment list prepares one statement per status filter
        // so a given status is an equality the planner can seek through the
        // status column of `idx_provider_target_grants_environment_v1`, and
        // the unfiltered list walks an index without that column.
        shape(
            "provider_target_grants_for_environment_page all",
            "provider_target_grants",
            &["idx_provider_target_grants_environment_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(3, KeysetDirection::Descending, "created_at", "grant_id"),
            environment_page_sql::all,
        ),
        shape(
            "provider_target_grants_for_environment_page by status",
            "provider_target_grants",
            &["idx_provider_target_grants_environment_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(4, KeysetDirection::Descending, "created_at", "grant_id"),
            environment_page_sql::by_status,
        ),
        // The global history prepares one statement per filter shape so
        // each filter is an equality the planner can seek.
        shape(
            "provider_target_grant_history_page unfiltered",
            "provider_target_grants",
            &["idx_provider_target_grants_history_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(1, KeysetDirection::Descending, "created_at", "grant_id"),
            history_page_sql::unfiltered,
        ),
        shape(
            "provider_target_grant_history_page by status",
            "provider_target_grants",
            &["idx_provider_target_grants_status_history_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(2, KeysetDirection::Descending, "created_at", "grant_id"),
            history_page_sql::by_status,
        ),
        shape(
            "provider_target_grant_history_page by target",
            "provider_target_grants",
            &["idx_provider_target_grants_target_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(2, KeysetDirection::Descending, "created_at", "grant_id"),
            history_page_sql::by_target,
        ),
        shape(
            "provider_target_grant_history_page by target and status",
            "provider_target_grants",
            &["idx_provider_target_grants_target_history_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(3, KeysetDirection::Descending, "created_at", "grant_id"),
            history_page_sql::by_target_and_status,
        ),
        shape(
            "provider_target_grant_audit_events_page",
            "provider_target_grant_audit_events",
            &["sqlite_autoindex_provider_target_grant_audit_events_2"],
            Seek::Resumed("grant_revision"),
            value_and_identity(2, KeysetDirection::Ascending, "grant_revision", "event_id"),
            grant_audit_events_page_sql,
        ),
        shape(
            "provider credential bindings_page",
            "provider_credential_bindings",
            &["idx_provider_credential_bindings_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(1, KeysetDirection::Ascending, "created_at", "binding_id"),
            credential_bindings_page_sql,
        ),
        shape(
            "provider credential audit_events_page",
            "provider_credential_audit_events",
            &["idx_provider_credential_audit_binding"],
            Seek::Resumed("occurred_at"),
            value_and_identity(2, KeysetDirection::Ascending, "occurred_at", "event_id"),
            credential_audit_events_page_sql,
        ),
        shape(
            "provider driver releases_page",
            "provider_driver_releases",
            &["idx_provider_driver_releases_page_v1"],
            Seek::Resumed("created_at"),
            value_and_identity(
                2,
                KeysetDirection::Descending,
                "created_at",
                "release_sha256",
            ),
            releases_page_sql,
        ),
        shape(
            "provider driver release audit_events_page",
            "provider_driver_release_audit",
            &["idx_provider_driver_release_audit_release"],
            Seek::Resumed("observed_at"),
            value_and_identity(3, KeysetDirection::Ascending, "observed_at", "event_id"),
            release_audit_events_page_sql,
        ),
        // The lifecycle audit carries no explicit index: the table's own
        // `UNIQUE(resource_kind, resource_id, resource_revision)` is the
        // index this page seeks, and the sort's tie-breaker can never be
        // reached because that triple is unique.
        shape(
            "provider lifecycle audit_events_page",
            "provider_lifecycle_audit_events",
            &["sqlite_autoindex_provider_lifecycle_audit_events_2"],
            Seek::Resumed("resource_revision"),
            value_and_identity(
                3,
                KeysetDirection::Ascending,
                "resource_revision",
                "event_id",
            ),
            lifecycle_audit_events_page_sql,
        ),
    ])
}

/// The diagnostic histories, which page through `database::read_keyset_page`
/// rather than through a store of their own: the event log, the command
/// log, the configuration history and the heartbeat history, each in the
/// filter shapes the API offers.
fn diagnostic_queries() -> Vec<KeysetQuery> {
    fn case(
        name: &'static str,
        table: &'static str,
        columns: &'static str,
        sort_column: &'static str,
        filter_columns: &'static [&'static str],
        indexes: &'static [&'static str],
    ) -> PageShape {
        let build = |resumed| {
            keyset_page_sql(
                KeysetPageShape {
                    table,
                    columns,
                    sort_column,
                    filter_columns,
                },
                resumed,
            )
        };
        PageShape {
            name,
            table,
            indexes,
            seek: Seek::Resumed(sort_column),
            first: build(false),
            resumed: build(true),
        }
    }
    shapes(vec![
        // The unfiltered log is the one page whose sort an older index
        // already answers: `id` is the row id, so `events(timestamp)`
        // orders equal seconds by id as well, and with no statistics the
        // planner takes whichever of the two it meets first. Both are the
        // same single seek, so either is accepted here.
        case(
            "list_events_page unfiltered",
            "events",
            EVENT_COLUMNS,
            "timestamp",
            &[],
            &["idx_events_page_v1", "idx_events_timestamp"],
        ),
        case(
            "list_events_page by node",
            "events",
            EVENT_COLUMNS,
            "timestamp",
            &["node_id"],
            &["idx_events_node_page_v1"],
        ),
        case(
            "list_events_page by severity",
            "events",
            EVENT_COLUMNS,
            "timestamp",
            &["severity"],
            &["idx_events_severity_page_v1"],
        ),
        // Both filters at once: the schema carries no fourth index pairing
        // them and Control never runs ANALYZE, so the planner has no
        // statistics to tell it which partition is smaller and seeks one of
        // the two, comparing the other column row by row. Either is one
        // partition of the log rather than the whole of it; which one the
        // planner takes is recorded in the schema comment above the
        // indexes.
        case(
            "list_events_page by node and severity",
            "events",
            EVENT_COLUMNS,
            "timestamp",
            &["node_id", "severity"],
            &["idx_events_severity_page_v1", "idx_events_node_page_v1"],
        ),
        case(
            "list_commands_page unfiltered",
            "commands",
            COMMAND_COLUMNS,
            "created_at",
            &[],
            &["idx_commands_page_v1"],
        ),
        case(
            "list_commands_page by node",
            "commands",
            COMMAND_COLUMNS,
            "created_at",
            &["node_id"],
            &["idx_commands_node_page_v1"],
        ),
        case(
            "list_config_history_page by node",
            "config_history",
            CONFIG_HISTORY_COLUMNS,
            "created_at",
            &["node_id"],
            &["idx_config_history_node_page_v1"],
        ),
        case(
            "list_config_history_page by node and key",
            "config_history",
            CONFIG_HISTORY_COLUMNS,
            "created_at",
            &["node_id", "config_key"],
            &["idx_config_history_key_page_v1"],
        ),
        case(
            "list_heartbeats_page by node",
            "heartbeats",
            HEARTBEAT_COLUMNS,
            "timestamp",
            &["node_id"],
            &["idx_heartbeats_node"],
        ),
    ])
}

/// A realistic slice of every paged table on one in-memory database: an
/// application with versions, artifacts, environments, deployments, routes,
/// secrets and hostname grants; accounts with tokens; an audit trail; and a
/// provider installation with a target, a grant, a credential and a
/// deployment workflow.
async fn seeded_database() -> Arc<Database> {
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let store = CatalogStore::new(Arc::clone(&database));

    let app = store.create_app("plan-evidence").await.unwrap();
    let production = store
        .create_environment("plan-evidence", "production")
        .await
        .unwrap();
    for name in ["preview-a", "preview-b"] {
        store
            .create_environment("plan-evidence", name)
            .await
            .unwrap();
    }
    seed_versions_and_deployments(&database, &app.id, &production.id, 3, 40).await;
    // A hostname has to be granted before an environment may route it.
    for pattern in ["plan-evidence.example.com", "*.plan-evidence.example.net"] {
        store
            .create_domain_grant(pattern, "plan-evidence", "production", None)
            .await
            .unwrap();
    }
    for (prefix, priority) in [("/", 0), ("/api", 10), ("/static", 5)] {
        store
            .create_route(
                &production,
                Some("plan-evidence.example.com".to_string()),
                prefix,
                priority,
            )
            .await
            .unwrap();
    }
    for name in ["DATABASE_URL", "API_KEY", "SIGNING_KEY"] {
        store
            .put_secret(&production, name, vec![0xAB; 48])
            .await
            .unwrap();
    }
    store.create_app("plan-evidence-retired").await.unwrap();
    store.delete_app("plan-evidence-retired").await.unwrap();

    let identity = IdentityStore::new(Arc::clone(&database));
    for (name, role) in [("alice", Role::Owner), ("bob", Role::Maintainer)] {
        identity.create_account(name, role).await.unwrap();
        identity
            .create_token(name, "ci", Authority::Unscoped)
            .await
            .unwrap();
        identity
            .create_token(name, "laptop", Authority::Unscoped)
            .await
            .unwrap();
    }

    let audit = AuditStore::new(Arc::clone(&database));
    let alice = Principal {
        kind: PrincipalKind::Account,
        account: Some("alice".to_string()),
        role: Some(Role::Owner),
        credential_id: Some("0011223344556677".to_string()),
        authority: Authority::Unscoped,
    };
    for (action, outcome, status) in [
        ("deploy", Outcome::Accepted, 200),
        ("secret.write", Outcome::Accepted, 204),
        ("token.mint", Outcome::Refused, 403),
    ] {
        audit
            .record(
                AuditRecord::by(&alice)
                    .action(action)
                    .request("POST", "/api/apps/plan-evidence/environments/production")
                    .resource(
                        Some("plan-evidence".to_string()),
                        Some("production".to_string()),
                    )
                    .outcome(outcome, status)
                    .build(),
            )
            .await;
    }

    seed_diagnostic_rows(&database).await;
    seed_provider_rows(&database, &store, app.canonical_id, production.canonical_id).await;
    database
}

/// The diagnostic histories the platform surface pages: one connected node
/// with heartbeats, commands, configuration revisions and log entries of
/// more than one severity, written through the same entry points the
/// runtime channel writes them through.
async fn seed_diagnostic_rows(database: &Database) {
    let session = NodeSession::new(
        "plan-evidence-node".to_owned(),
        NodeInfo {
            node_role: NodeRole::Runtime("0.1.0".to_owned()),
            listen_host: "127.0.0.1".to_owned(),
            listen_port: vec![8080],
        },
    );
    database.save_node(&session).await.unwrap();
    for index in 0..4 {
        database
            .save_heartbeat(&HeartbeatEntry {
                node_id: session.node_id.clone(),
                instance_id: session.instance_id.clone(),
                timestamp: Utc::now() - chrono::Duration::seconds(index),
                latency_ms: Some(index + 1),
            })
            .await
            .unwrap();
    }
    for (index, command_type) in ["deploy", "reload", "drain"].into_iter().enumerate() {
        database
            .save_command(&CommandEntry {
                id: uuid::Uuid::new_v4().to_string(),
                node_id: session.node_id.clone(),
                command_type: command_type.to_owned(),
                command_data: serde_json::json!({ "sequence": index }),
                status: CommandStatus::Pending,
                created_at: Utc::now(),
                sent_at: None,
                completed_at: None,
                timeout_at: None,
                response_data: None,
                error_message: None,
            })
            .await
            .unwrap();
    }
    for (event_type, severity) in [
        ("node.connected", EventSeverity::Info),
        ("deployment.applied", EventSeverity::Info),
        ("deployment.refused", EventSeverity::Error),
    ] {
        database
            .save_event(&Event {
                event_type: event_type.to_owned(),
                node_id: Some(session.node_id.clone()),
                severity,
                message: format!("{event_type} on the plan evidence node"),
                metadata: None,
            })
            .await
            .unwrap();
    }
    // Twice, so the second round records `update` revisions of the same
    // keys and the history has more than one row per key.
    for round in 0..2 {
        let configs = std::collections::HashMap::from([
            (
                "runtime.concurrency".to_owned(),
                serde_json::json!(4 + round),
            ),
            ("runtime.log_level".to_owned(), serde_json::json!("info")),
        ]);
        database
            .save_configs(&session.node_id, &configs, Utc::now())
            .await
            .unwrap();
    }
}

/// `versions` and `deployments` are written directly: the public deploy path
/// needs live runtime nodes, and the rows it writes are exactly these.
async fn seed_versions_and_deployments(
    database: &Database,
    app_id: &str,
    env_id: &str,
    versions: usize,
    deployments: usize,
) -> Vec<CanonicalId> {
    let version_ids = seed_versions(database, app_id, versions).await;
    seed_deployments(database, app_id, env_id, &version_ids, deployments).await
}

/// `versions` numbered from 1, each with its own canonical artifact. Native
/// bundle versions carry no provider lineage; the artifact rows stand on
/// their own for the artifact page. (A version may name both lineage
/// columns or neither; the schema trigger refuses one without the other.)
async fn seed_versions(database: &Database, app_id: &str, versions: usize) -> Vec<String> {
    let app_id = app_id.to_string();
    database
        .with_connection(move |connection| {
            let transaction = connection.unchecked_transaction()?;
            let base = now_secs() - 86_400;
            let mut version_ids = Vec::new();
            for number in 1..=versions {
                let version_id = uuid::Uuid::new_v4().to_string();
                let artifact_id = CanonicalId::new_v7().to_string();
                transaction.execute(
                    "INSERT INTO canonical_artifacts
                       (id, app_id, artifact_sha256, content_root_sha256,
                        standard_version, file_count, storage_path, record, created_at)
                     VALUES (?1, ?2, ?3, ?4, 'edge-canon@1', 3, '/artifacts/plan', '{}', ?5)",
                    params![
                        artifact_id,
                        app_id,
                        format!("{number:064x}"),
                        format!("{:064x}", number + 100),
                        base + number as i64,
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO versions
                       (id, canonical_id, app_id, number, bundle_sha, bundle_path,
                        manifest, canonical_artifact_id, provider_derivation_sha256, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, '/bundles/plan.zip', '{}', NULL, NULL, ?6)",
                    params![
                        version_id,
                        CanonicalId::new_v7().to_string(),
                        app_id,
                        number as i64,
                        format!("{:064x}", number + 200),
                        base + number as i64,
                    ],
                )?;
                version_ids.push(version_id);
            }
            transaction.commit()?;
            Ok::<_, DatabaseError>(version_ids)
        })
        .await
        .unwrap()
}

/// Deployment pointers for one environment, cycling through `version_ids`,
/// seven to a second so the tie-breaker is exercised. Returns the canonical
/// ids in insertion order.
async fn seed_deployments(
    database: &Database,
    app_id: &str,
    env_id: &str,
    version_ids: &[String],
    deployments: usize,
) -> Vec<CanonicalId> {
    let app_id = app_id.to_string();
    let env_id = env_id.to_string();
    let version_ids = version_ids.to_vec();
    database
        .with_connection(move |connection| {
            let transaction = connection.unchecked_transaction()?;
            let base = now_secs() - 86_400;
            let mut insert = transaction.prepare(
                "INSERT INTO deployments
                   (id, canonical_id, app_id, env_id, version_id, created_at, note, snapshot_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)",
            )?;
            let mut canonical_ids = Vec::with_capacity(deployments);
            for index in 0..deployments {
                let canonical_id = CanonicalId::new_v7();
                insert.execute(params![
                    uuid::Uuid::new_v4().to_string(),
                    canonical_id.to_string(),
                    app_id,
                    env_id,
                    version_ids[index % version_ids.len()],
                    // Ties on the second are deliberate: the tie-breaker is
                    // what the index and the cursor have to agree on.
                    base + (index / 7) as i64,
                ])?;
                canonical_ids.push(canonical_id);
            }
            drop(insert);
            transaction.commit()?;
            Ok::<_, DatabaseError>(canonical_ids)
        })
        .await
        .unwrap()
}

fn provider_manifest() -> ValidatedProviderDriverManifest {
    let mut manifest: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
        "../../../../contracts/v1/fixtures/valid/provider-driver.json"
    ))
    .unwrap();
    manifest
        .supported_standard_versions
        .push(SupportedStandard {
            contract_id: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION
                .split_once('@')
                .unwrap()
                .0
                .to_owned(),
            version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
            contract_digest: ToolDigest {
                algorithm: "sha256".to_owned(),
                value: deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256
                    .parse()
                    .unwrap(),
            },
        });
    ValidatedProviderDriverManifest::new(manifest).unwrap()
}

fn operation_context(key: &str) -> OperationRequestContext {
    OperationRequestContext::new(
        key,
        "POST\n/provider-operations\nv1",
        "account:plan-evidence".to_owned(),
        CanonicalId::new_v7(),
        CanonicalId::new_v7(),
    )
    .unwrap()
}

async fn seed_provider_rows(
    database: &Arc<Database>,
    store: &CatalogStore,
    application_id: CanonicalId,
    environment_id: CanonicalId,
) {
    let vault = CatalogProviderCredentialVault::new(
        store.clone(),
        Arc::new(SecretsBackend::Ready(Box::new(
            SecretCipher::from_key_bytes(&[7; 32]).unwrap(),
        ))),
    );
    let credential = vault
        .provision(
            ProviderCredentialBindingSpec {
                backend_id: "cloudflare-workers-pages".to_owned(),
                name: "plan-evidence".to_owned(),
            },
            CredentialMaterial::new(b"plan-evidence-credential".to_vec()).unwrap(),
            operation_context("plan-evidence-credential"),
        )
        .await
        .unwrap();
    let installation = store
        .register_provider_installation(
            ProviderInstallationSpec {
                backend_id: "cloudflare-workers-pages".to_owned(),
                account_reference: "account-reference".to_owned(),
                region: None,
                credential_binding_id: credential.binding_id,
            },
            None,
        )
        .await
        .unwrap();
    let manifest = provider_manifest();
    super::provider_driver_release::seed_test_provider_driver_release(store, &manifest).await;
    let target = store
        .register_provider_target(
            ProviderTargetSpec {
                installation_id: installation.installation_id,
                name: "production".to_owned(),
                standard_version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                target_config_digest: "b".repeat(64).parse().unwrap(),
                driver_manifest: manifest,
            },
            None,
        )
        .await
        .unwrap();
    store
        .grant_provider_target(
            ProviderTargetGrantSpec {
                application_id,
                environment_id,
                provider_target_id: target.target_id,
                note: Some("plan evidence".to_owned()),
            },
            ProviderTargetGrantMutationContext {
                actor_subject: "account:plan-evidence-admin".to_owned(),
                idempotency_record: None,
            },
        )
        .await
        .unwrap();

    // A provider deployment needs an artifact version with a provider
    // derivation; written the way the artifact pipeline writes them.
    let standard_version = target.standard_version.clone();
    let artifact_id = CanonicalId::new_v7();
    let artifact_version_id = CanonicalId::new_v7();
    let canonical_sha256: Sha256Digest = "1".repeat(64).parse().unwrap();
    let provider_sha256: Sha256Digest = "2".repeat(64).parse().unwrap();
    let (canonical_text, provider_text) = (
        canonical_sha256.as_str().to_owned(),
        provider_sha256.as_str().to_owned(),
    );
    database
        .with_connection(move |connection| {
            let app_id: String = connection.query_row(
                "SELECT id FROM apps WHERE canonical_id = ?1",
                [application_id.to_string()],
                |row| row.get(0),
            )?;
            connection.execute(
                "INSERT INTO canonical_artifacts
                   (id, app_id, artifact_sha256, content_root_sha256,
                    standard_version, file_count, storage_path, record, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, '/artifacts/provider', '{}', ?6)",
                params![
                    artifact_id.to_string(),
                    app_id,
                    canonical_text,
                    "3".repeat(64),
                    standard_version,
                    now_secs(),
                ],
            )?;
            for (manifest_sha256, provider, payload_root) in [
                ("4".repeat(64), "deislet", "5".repeat(64)),
                ("6".repeat(64), "cloudflare", provider_text),
            ] {
                connection.execute(
                    "INSERT INTO provider_derivations
                       (manifest_sha256, artifact_id, provider, standard_version,
                        payload_root_sha256, manifest, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, '{}', ?6)",
                    params![
                        manifest_sha256,
                        artifact_id.to_string(),
                        provider,
                        standard_version,
                        payload_root,
                        now_secs(),
                    ],
                )?;
            }
            connection.execute(
                "INSERT INTO versions
                   (id, canonical_id, app_id, number, bundle_sha, bundle_path,
                    manifest, canonical_artifact_id, provider_derivation_sha256, created_at)
                 VALUES (?1, ?2, ?3, 99, ?4, '/bundles/provider.zip', '{}', ?5, ?6, ?7)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    artifact_version_id.to_string(),
                    app_id,
                    "7".repeat(64),
                    artifact_id.to_string(),
                    "4".repeat(64),
                    now_secs(),
                ],
            )?;
            Ok::<(), DatabaseError>(())
        })
        .await
        .unwrap();
    // A disable and the enable that undoes it, so the lifecycle audit trail
    // this file explains has rows of its own. The installation is left
    // active, which is what the deployment below needs.
    let lifecycle = || ProviderLifecycleMutationContext {
        actor_subject: "account:plan-evidence-admin".to_owned(),
        idempotency_record: None,
    };
    let disabled = store
        .disable_provider_installation(
            installation.installation_id,
            installation.revision,
            lifecycle(),
        )
        .await
        .unwrap();
    store
        .enable_provider_installation(disabled.installation_id, disabled.revision, lifecycle())
        .await
        .unwrap();

    store
        .begin_provider_deployment(
            ProviderDeploymentSpec {
                application_id,
                environment_id,
                provider_target_id: target.target_id,
                artifact: ArtifactVersionRef {
                    artifact_version_id,
                    canonical_artifact_sha256: canonical_sha256,
                    provider_payload_sha256: provider_sha256,
                },
                plan: ProviderNeutralPlan::default(),
            },
            operation_context("plan-evidence-deployment"),
        )
        .await
        .unwrap();
}

async fn plans_for(
    database: &Database,
    queries: Vec<KeysetQuery>,
) -> Vec<(KeysetQuery, Vec<String>)> {
    database
        .with_connection(move |connection| {
            let plans = queries
                .into_iter()
                .map(|query| {
                    let plan = explain(connection, &query.sql);
                    (query, plan)
                })
                .collect::<Vec<_>>();
            Ok::<_, DatabaseError>(plans)
        })
        .await
        .unwrap()
}

async fn assert_all(queries: Vec<KeysetQuery>) {
    let database = seeded_database().await;
    for (query, plan) in plans_for(&database, queries).await {
        eprintln!("{}:\n  {}", query.name, plan.join("\n  "));
        assert_keyset_plan(&query, &plan);
    }
}

#[tokio::test]
async fn catalog_page_queries_walk_their_indexes() {
    assert_all(catalog_queries()).await;
}

#[tokio::test]
async fn identity_page_queries_walk_their_indexes() {
    assert_all(identity_queries()).await;
}

#[tokio::test]
async fn audit_page_queries_walk_their_indexes() {
    assert_all(audit_queries()).await;
}

#[tokio::test]
async fn provider_page_queries_walk_their_indexes() {
    assert_all(provider_queries()).await;
}

#[tokio::test]
async fn diagnostic_page_queries_walk_their_indexes() {
    assert_all(diagnostic_queries()).await;
}

/// The seeded rows are real enough that the paged stores read them back
/// through their public entry points; otherwise the plans above would be
/// about tables the product never fills.
#[tokio::test]
async fn seeded_rows_are_readable_through_the_stores() {
    let database = seeded_database().await;
    let store = CatalogStore::new(Arc::clone(&database));
    let production = store
        .get_environment("plan-evidence", "production")
        .await
        .unwrap();
    assert_eq!(
        store
            .list_deployments_page(&production.id, 10, None)
            .await
            .unwrap()
            .total,
        40
    );
    assert_eq!(
        store
            .list_versions_page("plan-evidence", 10, None)
            .await
            .unwrap()
            .total,
        4
    );
    assert_eq!(
        store
            .list_routes_page(&production.id, 10, None)
            .await
            .unwrap()
            .total,
        3
    );
    assert_eq!(
        store
            .list_secrets_page(&production.id, 10, None)
            .await
            .unwrap()
            .total,
        3
    );
    assert_eq!(
        store.list_domain_grants_page(10, None).await.unwrap().total,
        2
    );
    assert_eq!(
        store.list_retired_names_page(10, None).await.unwrap().total,
        1
    );
    assert_eq!(
        store
            .list_canonical_artifacts_page("plan-evidence", 10, None)
            .await
            .unwrap()
            .total,
        4
    );
    assert_eq!(
        store
            .provider_installations_page(10, None)
            .await
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        store
            .provider_targets_page(None, 10, None)
            .await
            .unwrap()
            .total,
        1
    );
    let installation = store
        .provider_installations_page(10, None)
        .await
        .unwrap()
        .items
        .remove(0);
    // The disable and the enable that undid it.
    assert_eq!(
        store
            .provider_lifecycle_audit_events_page(
                ProviderLifecycleResourceKind::Installation,
                installation.installation_id,
                10,
                None,
            )
            .await
            .unwrap()
            .total,
        2
    );
    assert_eq!(
        database
            .list_events_page(None, None, 10, None)
            .await
            .unwrap()
            .total,
        3
    );
    assert_eq!(
        database
            .list_commands_page(Some("plan-evidence-node"), 10, None)
            .await
            .unwrap()
            .total,
        3
    );
    assert_eq!(
        database
            .list_config_history_page("plan-evidence-node", None, 10, None)
            .await
            .unwrap()
            .total,
        4
    );
    assert_eq!(
        database
            .list_heartbeats_page("plan-evidence-node", 10, None)
            .await
            .unwrap()
            .total,
        4
    );
    let identity = IdentityStore::new(Arc::clone(&database));
    assert_eq!(
        identity.list_accounts_page(10, None).await.unwrap().total,
        2
    );
    let audit = AuditStore::new(database);
    assert_eq!(
        audit
            .query_page(AuditQuery::default(), 10, None)
            .await
            .unwrap()
            .total,
        3
    );
}

/// Load evidence for the deployment history page.
///
/// 50,000 deployment pointers over five environments, paged through one
/// environment (10,000 rows) at the maximum page size, each page resumed
/// from the cursor the previous one issued and resolved through the same
/// cursor table the API uses. Only correctness is asserted - every row
/// exactly once, the total exact on every page - and the per-page timings
/// are printed for the report. Ignored by default because it writes 50,000
/// rows; run it with:
///
/// ```text
/// cargo test -p deis-control --lib pagination_plans::deployments_page_walks_a_large_environment -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "load evidence: inserts 50,000 rows; run explicitly with --ignored --nocapture"]
async fn deployments_page_walks_a_large_environment() {
    const ENVIRONMENTS: usize = 5;
    const DEPLOYMENTS_PER_ENVIRONMENT: usize = 10_000;
    const PAGE: usize = 1_000;

    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let store = CatalogStore::new(Arc::clone(&database));
    let app = store.create_app("load-evidence").await.unwrap();
    let mut environments = Vec::new();
    for index in 0..ENVIRONMENTS {
        environments.push(
            store
                .create_environment("load-evidence", &format!("env-{index}"))
                .await
                .unwrap(),
        );
    }
    let seeding = Instant::now();
    let version_ids = seed_versions(&database, &app.id, 5).await;
    let mut expected = Vec::new();
    for (index, environment) in environments.iter().enumerate() {
        let ids = seed_deployments(
            &database,
            &app.id,
            &environment.id,
            &version_ids,
            DEPLOYMENTS_PER_ENVIRONMENT,
        )
        .await;
        if index == 2 {
            expected = ids;
        }
    }
    eprintln!(
        "seeded {} deployments in {:?}",
        ENVIRONMENTS * DEPLOYMENTS_PER_ENVIRONMENT,
        seeding.elapsed()
    );

    let target = &environments[2];
    let cursors = PageCursorStore::new(Arc::clone(&database));
    let collection = "deployments";
    let binding = format!("scope=environment:{}", target.canonical_id);
    let mut seen = std::collections::HashSet::with_capacity(DEPLOYMENTS_PER_ENVIRONMENT);
    let mut token: Option<String> = None;
    let mut pages = 0;
    let mut previous: Option<(i64, String)> = None;
    let walk = Instant::now();
    loop {
        let page_timer = Instant::now();
        let position = match &token {
            Some(token) => Some(cursors.resolve(token, collection, &binding).await.unwrap()),
            None => None,
        };
        let page = store
            .list_deployments_page(&target.id, PAGE, position)
            .await
            .unwrap();
        pages += 1;
        assert_eq!(page.total, DEPLOYMENTS_PER_ENVIRONMENT);
        assert!(!page.items.is_empty());
        for deployment in &page.items {
            assert_eq!(deployment.env_id, target.id);
            let key = (deployment.created_at, deployment.canonical_id.to_string());
            if let Some(previous) = &previous {
                assert!(
                    *previous > key,
                    "page order broke between {previous:?} and {key:?}"
                );
            }
            previous = Some(key);
            assert!(
                seen.insert(deployment.canonical_id),
                "deployment {} was returned twice",
                deployment.canonical_id
            );
        }
        let issue_timer = Instant::now();
        token = if page.has_more {
            let last = page.items.last().unwrap();
            Some(
                cursors
                    .issue(
                        collection,
                        &binding,
                        PagePosition {
                            sort_value: last.created_at,
                            identity: last.canonical_id.to_string(),
                            secondary: None,
                        },
                    )
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        eprintln!(
            "page {pages}: {} rows in {:?} (cursor issue {:?})",
            page.items.len(),
            page_timer.elapsed(),
            issue_timer.elapsed()
        );
        if token.is_none() {
            break;
        }
    }
    eprintln!(
        "walked {} rows in {} pages, {:?} total",
        seen.len(),
        pages,
        walk.elapsed()
    );
    assert_eq!(pages, DEPLOYMENTS_PER_ENVIRONMENT.div_ceil(PAGE));
    assert_eq!(seen.len(), DEPLOYMENTS_PER_ENVIRONMENT);
    let expected: std::collections::HashSet<_> = expected.into_iter().collect();
    assert_eq!(seen, expected);
}
