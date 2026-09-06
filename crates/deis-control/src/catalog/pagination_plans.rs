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
//! These tests ask SQLite itself. Every `*_page` query is prepared against
//! a database seeded with realistic rows and `EXPLAIN QUERY PLAN` has to
//! name the intended index and never fall back to a temporary sort or a
//! bare table scan. Where a store exposes its statement as a constant the
//! constant itself is explained; where it builds the statement inline, the
//! SQL text is copied here and a source guard checks that the same fragment
//! (the statement from `FROM` to `LIMIT`) appears both in the copy under
//! test and verbatim in the file it came from, so the evidence cannot drift
//! away from the query it is evidence for in either direction.
//!
//! Only the items query is checked. The `COUNT(*)` that accompanies it may
//! legitimately scan the scope.

use std::sync::Arc;
use std::time::Instant;

use deis_provider_driver::{
    ArtifactVersionRef, CredentialMaterial, ProviderDriverManifest, ProviderNeutralPlan,
    Sha256Digest, SupportedStandard, ToolDigest, ValidatedProviderDriverManifest,
};
use deis_wire::CanonicalId;
use rusqlite::{Connection, params};

use super::pagination::{PageCursorStore, PagePosition};
use super::provider_target_grant::history_page_sql;
use super::{
    CatalogProviderCredentialVault, CatalogStore, OperationRequestContext,
    ProviderCredentialBindingSpec, ProviderDeploymentSpec, ProviderInstallationSpec,
    ProviderTargetGrantMutationContext, ProviderTargetGrantSpec, ProviderTargetSpec, SecretCipher,
    SecretsBackend, now_secs,
};
use crate::audit::{AuditQuery, AuditRecord, AuditStore, Outcome};
use crate::database::{Database, DatabaseError};
use crate::identity::{Authority, IdentityStore, Principal, PrincipalKind, Role};

const STORE_SOURCE: &str = include_str!("store.rs");
const IDENTITY_SOURCE: &str = include_str!("../identity/store.rs");
const AUDIT_SOURCE: &str = include_str!("../audit.rs");
const PROVIDER_OPERATION_SOURCE: &str = include_str!("provider_driver_operation.rs");
const PROVIDER_GRANT_SOURCE: &str = include_str!("provider_target_grant.rs");
const PROVIDER_CREDENTIAL_SOURCE: &str = include_str!("provider_credential.rs");
const PROVIDER_RELEASE_SOURCE: &str = include_str!("provider_driver_release.rs");

/// One keyset query exactly as a store prepares it.
struct KeysetQuery {
    name: &'static str,
    /// The table (or its alias in the query) whose rows are being paged.
    table: &'static str,
    sql: String,
    /// Any one of these must appear in the plan. Two names where the query
    /// is written so that either index satisfies the sort.
    indexes: &'static [&'static str],
    /// What ties `sql` to the statement the store really prepares.
    provenance: Provenance,
    /// A plan step this query is known to still take and should not. The
    /// index assertion holds; the sort assertion is inverted so the marker
    /// is removed the moment the query is fixed, instead of quietly
    /// outliving the problem.
    known_shortfall: Option<&'static str>,
}

/// Where the SQL under test came from.
enum Provenance {
    /// `sql` is the store's own constant; nothing can drift.
    Store,
    /// `sql` is a copy of a statement the store writes inline. `fragment`
    /// is the statement from `FROM` to `LIMIT` and must appear, whitespace
    /// normalised, both in `sql` and in `source`, the file it was copied
    /// from. Either side changing without the other fails the guard.
    Copied {
        source: &'static str,
        fragment: &'static str,
    },
    /// `sql` was assembled by the same rules as a store that builds its
    /// statement at run time. `template` is the cursor clause as `source`
    /// spells it (placeholders unrendered) and `rendered` is that clause as
    /// it appears in `sql`.
    Built {
        source: &'static str,
        template: &'static str,
        rendered: String,
    },
}

fn normalise(text: &str) -> String {
    text.replace('\\', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

fn assert_keyset_plan(query: &KeysetQuery, plan: &[String]) {
    let text = plan.join("\n");
    match &query.provenance {
        Provenance::Store => {}
        Provenance::Copied { source, fragment } => {
            assert!(
                normalise(&query.sql).contains(&normalise(fragment)),
                "{}: the explained SQL does not contain its own source fragment, so the plan \
                 would be evidence about a statement nobody prepares. Fragment:\n{fragment}",
                query.name
            );
            assert!(
                normalise(source).contains(&normalise(fragment)),
                "{}: the SQL under test no longer appears in its source file; update the copy \
                 here so the plan evidence stays about the real query. Missing fragment:\n{fragment}",
                query.name
            );
        }
        Provenance::Built {
            source,
            template,
            rendered,
        } => {
            assert!(
                normalise(source).contains(&normalise(template)),
                "{}: the store no longer builds its cursor clause from this template; update \
                 the builder here to match. Missing template:\n{template}",
                query.name
            );
            assert!(
                normalise(&query.sql).contains(&normalise(rendered)),
                "{}: the explained SQL does not contain the rendered cursor clause:\n{rendered}",
                query.name
            );
        }
    }
    assert!(
        query.indexes.iter().any(|index| text.contains(index)),
        "{}: plan does not use any of {:?}:\n{text}",
        query.name,
        query.indexes
    );
    match query.known_shortfall {
        None => assert!(
            !text.contains("TEMP B-TREE"),
            "{}: plan sorts with a temporary b-tree instead of walking the index:\n{text}",
            query.name
        ),
        Some(shortfall) => assert!(
            text.contains(shortfall),
            "{}: the recorded shortfall {shortfall:?} is no longer in the plan; the query \
             was fixed, so drop `known_shortfall` from this case:\n{text}",
            query.name
        ),
    }
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
}

/// Every page query of the catalog store (`store.rs`).
fn catalog_queries() -> Vec<KeysetQuery> {
    vec![
        KeysetQuery {
            name: "list_apps_page",
            table: "apps",
            sql: "SELECT id, canonical_id, revision, created_at FROM apps
                  WHERE (?1 IS NULL OR id > ?1)
                  ORDER BY id ASC LIMIT ?2"
                .to_string(),
            indexes: &["sqlite_autoindex_apps_1"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM apps WHERE (?1 IS NULL OR id > ?1) ORDER BY id ASC LIMIT ?2",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_retired_names_page",
            table: "retired_app_names",
            sql: "SELECT app_id, retired_at, environments, released_at FROM retired_app_names
                  WHERE (?1 IS NULL OR app_id > ?1)
                  ORDER BY app_id ASC LIMIT ?2"
                .to_string(),
            indexes: &["sqlite_autoindex_retired_app_names_1"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM retired_app_names WHERE (?1 IS NULL OR app_id > ?1) \
                              ORDER BY app_id ASC LIMIT ?2",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_canonical_artifacts_page",
            table: "canonical_artifacts",
            sql: "SELECT id, app_id, artifact_sha256, content_root_sha256, standard_version, \
                  file_count, storage_path, record, created_at FROM canonical_artifacts
                  WHERE app_id = ?1
                    AND (?2 IS NULL OR created_at < ?2
                         OR (created_at = ?2 AND id < ?3))
                  ORDER BY created_at DESC, id DESC LIMIT ?4"
                .to_string(),
            indexes: &["idx_canonical_artifacts_page_v1"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM canonical_artifacts WHERE app_id = ?1 AND (?2 IS NULL OR \
                              created_at < ?2 OR (created_at = ?2 AND id < ?3)) ORDER BY \
                              created_at DESC, id DESC LIMIT ?4",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_versions_page",
            table: "versions",
            sql: "SELECT id, canonical_id, app_id, number, bundle_sha, bundle_path, manifest, \
                  canonical_artifact_id, provider_derivation_sha256, created_at FROM versions
                  WHERE app_id = ?1
                    AND (?2 IS NULL OR number < ?2
                         OR (number = ?2 AND id < ?3))
                  ORDER BY number DESC, id DESC LIMIT ?4"
                .to_string(),
            // `UNIQUE(app_id, number)` is the index that serves this page:
            // SQLite knows the pair is unique, so `id DESC` after `number
            // DESC` needs no sort. `idx_versions_app` would do as well.
            indexes: &["sqlite_autoindex_versions_2", "idx_versions_app"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM versions WHERE app_id = ?1 AND (?2 IS NULL OR number < ?2 \
                              OR (number = ?2 AND id < ?3)) ORDER BY number DESC, id DESC \
                              LIMIT ?4",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_environments_page",
            table: "environments",
            sql: "SELECT id, canonical_id, app_id, name, created_at FROM environments
                  WHERE app_id = ?1 AND (?2 IS NULL OR name > ?2)
                  ORDER BY name ASC LIMIT ?3"
                .to_string(),
            // `UNIQUE(app_id, name)`; the first autoindex is the primary key.
            indexes: &["sqlite_autoindex_environments_2"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM environments WHERE app_id = ?1 AND (?2 IS NULL OR name > ?2) \
                              ORDER BY name ASC LIMIT ?3",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_deployments_page",
            table: "deployments",
            sql: "SELECT id, canonical_id, app_id, env_id, version_id, created_at, note, \
                  snapshot_id FROM deployments
                  WHERE env_id = ?1
                    AND (?2 IS NULL OR created_at < ?2
                         OR (created_at = ?2 AND canonical_id < ?3))
                  ORDER BY created_at DESC, canonical_id DESC LIMIT ?4"
                .to_string(),
            indexes: &["idx_deployments_env_page_v1"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM deployments WHERE env_id = ?1 AND (?2 IS NULL OR created_at \
                              < ?2 OR (created_at = ?2 AND canonical_id < ?3)) ORDER BY \
                              created_at DESC, canonical_id DESC LIMIT ?4",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_routes_page",
            table: "routes",
            sql: "SELECT id, app_id, env_id, host, path_prefix, priority FROM routes
                  WHERE env_id = ?1
                    AND (?2 IS NULL OR priority < ?2
                         OR (priority = ?2 AND path_prefix > ?3)
                         OR (priority = ?2 AND path_prefix = ?3 AND id > ?4))
                  ORDER BY priority DESC, path_prefix ASC, id ASC LIMIT ?5"
                .to_string(),
            indexes: &["idx_routes_env_page_v1"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM routes WHERE env_id = ?1 AND (?2 IS NULL OR priority < ?2 OR \
                              (priority = ?2 AND path_prefix > ?3) OR (priority = ?2 AND \
                              path_prefix = ?3 AND id > ?4)) ORDER BY priority DESC, \
                              path_prefix ASC, id ASC LIMIT ?5",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_domain_grants_page",
            table: "domain_grants",
            sql: "SELECT pattern, app_id, environment, created_at, note FROM domain_grants
                  WHERE (?1 IS NULL OR (pattern, app_id, environment) > (?1, ?2, ?3))
                  ORDER BY pattern ASC, app_id ASC, environment ASC LIMIT ?4"
                .to_string(),
            indexes: &["sqlite_autoindex_domain_grants_1"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM domain_grants WHERE (?1 IS NULL OR (pattern, app_id, \
                              environment) > (?1, ?2, ?3)) ORDER BY pattern ASC, app_id ASC, \
                              environment ASC LIMIT ?4",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_secrets_page",
            table: "secret_heads",
            sql: "SELECT name, revision_id, updated_at FROM secret_heads
                  WHERE env_id = ?1 AND (?2 IS NULL OR name > ?2)
                  ORDER BY name ASC LIMIT ?3"
                .to_string(),
            indexes: &["sqlite_autoindex_secret_heads_1"],
            provenance: Provenance::Copied {
                source: STORE_SOURCE,
                fragment: "FROM secret_heads WHERE env_id = ?1 AND (?2 IS NULL OR name > ?2) \
                              ORDER BY name ASC LIMIT ?3",
            },
            known_shortfall: None,
        },
    ]
}

/// The identity store's page queries (`identity/store.rs`).
fn identity_queries() -> Vec<KeysetQuery> {
    vec![
        KeysetQuery {
            name: "list_accounts_page",
            table: "accounts",
            sql: "SELECT id, name, role, created_at, disabled_at FROM accounts
                  WHERE (?1 IS NULL OR created_at > ?1
                         OR (created_at = ?1 AND name > ?2))
                  ORDER BY created_at ASC, name ASC LIMIT ?3"
                .to_string(),
            indexes: &["idx_accounts_page_v1"],
            provenance: Provenance::Copied {
                source: IDENTITY_SOURCE,
                fragment: "FROM accounts WHERE (?1 IS NULL OR created_at > ?1 OR (created_at \
                              = ?1 AND name > ?2)) ORDER BY created_at ASC, name ASC LIMIT ?3",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "list_tokens_page",
            table: "t",
            sql: "SELECT t.id, t.account_id, t.name, t.origin, t.scopes, t.unscoped, \
                  t.created_at, t.last_used_at, t.revoked_at, a.name FROM api_tokens t
                   JOIN accounts a ON a.id = t.account_id
                  WHERE (?1 IS NULL OR t.account_id = ?1)
                    AND (?2 = 1 OR t.revoked_at IS NULL)
                    AND (?3 = 0 OR t.unscoped = 1)
                    AND (?4 IS NULL OR t.created_at > ?4
                         OR (t.created_at = ?4 AND t.id > ?5))
                  ORDER BY t.created_at ASC, t.id ASC LIMIT ?6"
                .to_string(),
            indexes: &["idx_api_tokens_page_v1", "idx_api_tokens_account_page_v1"],
            provenance: Provenance::Copied {
                source: IDENTITY_SOURCE,
                fragment: "FROM api_tokens t JOIN accounts a ON a.id = t.account_id WHERE (?1 \
                              IS NULL OR t.account_id = ?1) AND (?2 = 1 OR t.revoked_at IS \
                              NULL) AND (?3 = 0 OR t.unscoped = 1) AND (?4 IS NULL OR \
                              t.created_at > ?4 OR (t.created_at = ?4 AND t.id > ?5)) ORDER BY \
                              t.created_at ASC, t.id ASC LIMIT ?6",
            },
            known_shortfall: None,
        },
    ]
}

/// `AuditStore::query_page` builds its statement from the filters present,
/// so the unfiltered log and each single-filter variant are distinct
/// statements with distinct intended indexes.
fn audit_queries() -> Vec<KeysetQuery> {
    const AUDIT_COLUMNS: &str = "id, at, actor_kind, actor_account, actor_credential, action, \
                                 app_id, environment, target, method, path, peer, outcome, \
                                 status, detail";
    /// The statement and the rendered cursor clause it contains.
    fn build(filters: &[&str]) -> (String, String) {
        let mut clauses: Vec<String> = filters
            .iter()
            .enumerate()
            .map(|(index, filter)| format!("{filter} ?{}", index + 1))
            .collect();
        let at_index = filters.len() + 1;
        let id_index = filters.len() + 2;
        let limit_index = filters.len() + 3;
        let cursor_clause = format!(
            "(?{at_index} IS NULL OR at < ?{at_index} \
               OR (at = ?{at_index} AND id < ?{id_index}))"
        );
        clauses.push(cursor_clause.clone());
        let sql = format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_log WHERE {} \
             ORDER BY at DESC, id DESC LIMIT ?{limit_index}",
            clauses.join(" AND ")
        );
        (sql, cursor_clause)
    }
    const CURSOR_TEMPLATE: &str = "(?{at_index} IS NULL OR at < ?{at_index} OR (at = ?{at_index} \
                                   AND id < ?{id_index}))";
    let case = |name: &'static str, filters: &[&str], indexes: &'static [&'static str]| {
        let (sql, rendered) = build(filters);
        KeysetQuery {
            name,
            table: "audit_log",
            sql,
            indexes,
            provenance: Provenance::Built {
                source: AUDIT_SOURCE,
                template: CURSOR_TEMPLATE,
                rendered,
            },
            known_shortfall: None,
        }
    };
    vec![
        case("audit query_page unfiltered", &[], &["idx_audit_page_v1"]),
        case(
            "audit query_page by app",
            &["app_id ="],
            &["idx_audit_app_page_v1"],
        ),
        case(
            "audit query_page by app and environment",
            &["app_id =", "environment ="],
            &["idx_audit_app_page_v1"],
        ),
        case(
            "audit query_page by account",
            &["actor_account ="],
            &["idx_audit_account_page_v1"],
        ),
        case(
            "audit query_page by outcome",
            &["outcome ="],
            &["idx_audit_outcome_page_v1"],
        ),
        case(
            "audit query_page by action",
            &["action ="],
            &["idx_audit_page_v1"],
        ),
        case("audit query_page since", &["at >="], &["idx_audit_page_v1"]),
        case(
            "audit query_page since and until",
            &["at >=", "at <="],
            &["idx_audit_page_v1"],
        ),
        case(
            "audit query_page by app since",
            &["app_id =", "at >="],
            &["idx_audit_app_page_v1"],
        ),
    ]
}

/// The provider lists: installations, targets, deployments, grants,
/// credential bindings, driver releases and their audit trails.
fn provider_queries() -> Vec<KeysetQuery> {
    vec![
        KeysetQuery {
            name: "provider_installations_page",
            table: "provider_installations",
            sql: "SELECT installation_id FROM provider_installations
                      WHERE ?1 IS NULL OR created_at > ?1
                         OR (created_at = ?1 AND installation_id > ?2)
                      ORDER BY created_at, installation_id LIMIT ?3"
                .to_string(),
            indexes: &["idx_provider_installations_page_v1"],
            provenance: Provenance::Copied {
                source: PROVIDER_OPERATION_SOURCE,
                fragment: "FROM provider_installations WHERE ?1 IS NULL OR created_at > ?1 OR \
                              (created_at = ?1 AND installation_id > ?2) ORDER BY created_at, \
                              installation_id LIMIT ?3",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider_targets_page",
            table: "provider_targets",
            sql: "SELECT target_id FROM provider_targets
                      WHERE (?1 IS NULL OR installation_id = ?1)
                        AND (?2 IS NULL OR created_at > ?2
                             OR (created_at = ?2 AND target_id > ?3))
                      ORDER BY created_at, target_id LIMIT ?4"
                .to_string(),
            indexes: &[
                "idx_provider_targets_page_v1",
                "idx_provider_targets_installation_page_v1",
            ],
            provenance: Provenance::Copied {
                source: PROVIDER_OPERATION_SOURCE,
                fragment: "FROM provider_targets WHERE (?1 IS NULL OR installation_id = ?1) \
                              AND (?2 IS NULL OR created_at > ?2 OR (created_at = ?2 AND \
                              target_id > ?3)) ORDER BY created_at, target_id LIMIT ?4",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider_deployments_for_environment_page",
            table: "provider_deployment_workflows",
            sql: "SELECT operation_id FROM provider_deployment_workflows
                      WHERE application_id = ?1 AND environment_id = ?2
                        AND (?3 IS NULL OR created_at < ?3
                             OR (created_at = ?3 AND operation_id < ?4))
                      ORDER BY created_at DESC, operation_id DESC LIMIT ?5"
                .to_string(),
            indexes: &["idx_provider_deployment_environment_page_v1"],
            provenance: Provenance::Copied {
                source: PROVIDER_OPERATION_SOURCE,
                fragment: "FROM provider_deployment_workflows WHERE application_id = ?1 AND \
                              environment_id = ?2 AND (?3 IS NULL OR created_at < ?3 OR \
                              (created_at = ?3 AND operation_id < ?4)) ORDER BY created_at \
                              DESC, operation_id DESC LIMIT ?5",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider_target_grants_for_environment_page",
            table: "provider_target_grants",
            sql: "SELECT grant_id FROM provider_target_grants
                      WHERE application_id = ?1 AND environment_id = ?2
                        AND (?3 IS NULL OR status = ?3)
                        AND (?4 IS NULL OR created_at < ?4
                             OR (created_at = ?4 AND grant_id < ?5))
                      ORDER BY created_at DESC, grant_id DESC LIMIT ?6"
                .to_string(),
            indexes: &["idx_provider_target_grants_environment_v1"],
            provenance: Provenance::Copied {
                source: PROVIDER_GRANT_SOURCE,
                fragment: "FROM provider_target_grants WHERE application_id = ?1 AND \
                              environment_id = ?2 AND (?3 IS NULL OR status = ?3) AND (?4 IS \
                              NULL OR created_at < ?4 OR (created_at = ?4 AND grant_id < ?5)) \
                              ORDER BY created_at DESC, grant_id DESC LIMIT ?6",
            },
            // The index is `(application_id, environment_id, status,
            // created_at DESC, grant_id DESC)`, but the status filter is
            // `(?3 IS NULL OR status = ?3)`, which is not an equality the
            // planner can seek on, so the index stops at the environment and
            // every page sorts that environment's grants. Fixing it means
            // either a statement per status filter (equality when a status
            // is given) or an index without the status column for the
            // unfiltered list; both live outside this module.
            known_shortfall: Some("USE TEMP B-TREE FOR ORDER BY"),
        },
        // The global history prepares one statement per filter shape so
        // each filter is an equality the planner can seek; these are the
        // store's own constants.
        KeysetQuery {
            name: "provider_target_grant_history_page unfiltered",
            table: "provider_target_grants",
            sql: history_page_sql::UNFILTERED.to_string(),
            indexes: &["idx_provider_target_grants_history_v1"],
            provenance: Provenance::Store,
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider_target_grant_history_page by status",
            table: "provider_target_grants",
            sql: history_page_sql::BY_STATUS.to_string(),
            indexes: &["idx_provider_target_grants_status_history_v1"],
            provenance: Provenance::Store,
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider_target_grant_history_page by target",
            table: "provider_target_grants",
            sql: history_page_sql::BY_TARGET.to_string(),
            indexes: &["idx_provider_target_grants_target_page_v1"],
            provenance: Provenance::Store,
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider_target_grant_history_page by target and status",
            table: "provider_target_grants",
            sql: history_page_sql::BY_TARGET_AND_STATUS.to_string(),
            indexes: &["idx_provider_target_grants_target_history_v1"],
            provenance: Provenance::Store,
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider_target_grant_audit_events_page",
            table: "provider_target_grant_audit_events",
            sql: "SELECT event_id, application_id, environment_id,
                            provider_target_id, grant_revision, actor_id,
                            event_type, occurred_at
                      FROM provider_target_grant_audit_events
                      WHERE grant_id = ?1
                        AND (?2 IS NULL OR grant_revision > ?2
                             OR (grant_revision = ?2 AND event_id > ?3))
                      ORDER BY grant_revision, event_id LIMIT ?4"
                .to_string(),
            indexes: &["sqlite_autoindex_provider_target_grant_audit_events_2"],
            provenance: Provenance::Copied {
                source: PROVIDER_GRANT_SOURCE,
                fragment: "FROM provider_target_grant_audit_events WHERE grant_id = ?1 AND (?2 \
                              IS NULL OR grant_revision > ?2 OR (grant_revision = ?2 AND \
                              event_id > ?3)) ORDER BY grant_revision, event_id LIMIT ?4",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider credential bindings_page",
            table: "provider_credential_bindings",
            sql: "SELECT binding_id FROM provider_credential_bindings
                      WHERE ?1 IS NULL OR created_at > ?1
                         OR (created_at = ?1 AND binding_id > ?2)
                      ORDER BY created_at, binding_id LIMIT ?3"
                .to_string(),
            indexes: &["idx_provider_credential_bindings_page_v1"],
            provenance: Provenance::Copied {
                source: PROVIDER_CREDENTIAL_SOURCE,
                fragment: "FROM provider_credential_bindings WHERE ?1 IS NULL OR created_at > \
                              ?1 OR (created_at = ?1 AND binding_id > ?2) ORDER BY created_at, \
                              binding_id LIMIT ?3",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider credential audit_events_page",
            table: "provider_credential_audit_events",
            sql: "SELECT event_id, binding_id, credential_revision, lease_id,
                            operation_id, owner_id, claim_epoch, actor_id,
                            event_type, disposition, occurred_at
                       FROM provider_credential_audit_events
                      WHERE binding_id = ?1
                        AND (?2 IS NULL OR occurred_at > ?2
                             OR (occurred_at = ?2 AND event_id > ?3))
                      ORDER BY occurred_at, event_id LIMIT ?4"
                .to_string(),
            indexes: &["idx_provider_credential_audit_binding"],
            provenance: Provenance::Copied {
                source: PROVIDER_CREDENTIAL_SOURCE,
                fragment: "FROM provider_credential_audit_events WHERE binding_id = ?1 AND (?2 \
                              IS NULL OR occurred_at > ?2 OR (occurred_at = ?2 AND event_id > \
                              ?3)) ORDER BY occurred_at, event_id LIMIT ?4",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider driver releases_page",
            table: "provider_driver_releases",
            sql: "SELECT release_sha256 FROM provider_driver_releases
                      WHERE host_operating_system = ?1
                        AND (?2 IS NULL OR created_at < ?2
                             OR (created_at = ?2 AND release_sha256 < ?3))
                      ORDER BY created_at DESC, release_sha256 DESC LIMIT ?4"
                .to_string(),
            indexes: &["idx_provider_driver_releases_page_v1"],
            provenance: Provenance::Copied {
                source: PROVIDER_RELEASE_SOURCE,
                fragment: "FROM provider_driver_releases WHERE host_operating_system = ?1 AND \
                              (?2 IS NULL OR created_at < ?2 OR (created_at = ?2 AND \
                              release_sha256 < ?3)) ORDER BY created_at DESC, release_sha256 \
                              DESC LIMIT ?4",
            },
            known_shortfall: None,
        },
        KeysetQuery {
            name: "provider driver release audit_events_page",
            table: "provider_driver_release_audit",
            sql: "SELECT event_id, event_type, detail, observed_at
                       FROM provider_driver_release_audit
                      WHERE release_sha256 = ?1 AND host_operating_system = ?2
                        AND (?3 IS NULL OR observed_at > ?3
                             OR (observed_at = ?3 AND event_id > ?4))
                      ORDER BY observed_at, event_id LIMIT ?5"
                .to_string(),
            indexes: &["idx_provider_driver_release_audit_release"],
            provenance: Provenance::Copied {
                source: PROVIDER_RELEASE_SOURCE,
                fragment: "FROM provider_driver_release_audit WHERE release_sha256 = ?1 AND \
                              host_operating_system = ?2 AND (?3 IS NULL OR observed_at > ?3 OR \
                              (observed_at = ?3 AND event_id > ?4)) ORDER BY observed_at, \
                              event_id LIMIT ?5",
            },
            known_shortfall: None,
        },
    ]
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

    seed_provider_rows(&database, &store, app.canonical_id, production.canonical_id).await;
    database
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
