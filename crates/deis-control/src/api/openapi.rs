//! The OpenAPI document of the human API: `/api/v1` (product) and
//! `/api/platform` (diagnostics), as `docs/adr/016-api-cli-web-console.md`
//! promises them.
//!
//! What is here and what is not, on purpose:
//!
//! * Every operation under the two prefixes is listed in [`ControlApi`], by
//!   hand, in the order the routers mount them. The document is opt-in:
//!   a handler that is not named here is not described, and a route that
//!   is described but not mounted is caught by `tests::` below.
//! * The legacy flat `/api/*` tree is the same handlers under a deprecated
//!   prefix and is not described separately.
//! * The node channel (`/api/nodes/*`, `/api/routing-table`, the bundle
//!   download) and the Prometheus exposition at `/api/metrics` are never in
//!   this document; they are not part of either contract.
//! * Types shared with other crates (`deis-wire`, `deis-provider-driver`,
//!   `deis-bundle`) are described by the `*Document` markers at the bottom
//!   of this file rather than by deriving a schema in those crates: their
//!   authority is the JSON Schema under `contracts/v1`, and the marker says
//!   which one.
//!
//! The rendered document lives at `contracts/openapi/v1.json`. The drift
//! test in `tests/openapi_drift.rs` keeps it equal to what this module
//! renders, and `scripts/check-openapi-compat.py` refuses a change that
//! breaks a client without a major version bump.

use std::borrow::Cow;

use serde::Serialize;
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::openapi::{KnownFormat, RefOr, SchemaFormat, SecurityRequirement};
use utoipa::{Modify, OpenApi, PartialSchema, ToResponse, ToSchema};

use deis_provider_driver::{DriverErrorCategory, SideEffect};

use super::error::{ProviderDiagnostic, codes};

/// The version of the document, not of the binary. `major` changes when a
/// client written against the previous document would break; the
/// compatibility gate holds every other change to that rule.
pub const DOCUMENT_VERSION: &str = "1.0.0";

/// Where the rendered document is committed, relative to the repository
/// root.
pub const CONTRACT_PATH: &str = "contracts/openapi/v1.json";

/// Where captured response examples are committed, beside the document.
pub const EXAMPLES_PATH: &str = "contracts/openapi/v1-examples.json";

/// The two tags. Every operation carries exactly one, and the tag decides
/// the prefix: `product` operations live under `/api/v1`, `platform`
/// operations under `/api/platform`.
pub const TAG_PRODUCT: &str = "product";
pub const TAG_PLATFORM: &str = "platform";

/// The security scheme names as they appear in the document.
pub const SECURITY_BEARER: &str = "bearerToken";
pub const SECURITY_API_KEY: &str = "apiKey";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Deislet Control API",
        version = DOCUMENT_VERSION,
        description = "The human API of the Deislet control plane: the product API under \
                       `/api/v1` and the platform diagnostics under `/api/platform`. \
                       Every list answers in the `ControlListPage` envelope with an opaque \
                       cursor; every error answers with the `ErrorBody` envelope, whose \
                       `code` is a closed set and whose `requestId` is echoed in the \
                       `x-request-id` header. Mutations that take time answer 202 with a \
                       `ProductOperation` and a `Location` to poll.",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    servers((url = "/", description = "The control plane that serves this document")),
    tags(
        (name = "product", description = "Applications and everything inside them, accounts and \
                                          tokens, the audit log, hostname grants, providers, \
                                          backend restores and long operations. Served under \
                                          `/api/v1`."),
        (name = "platform", description = "Operator diagnostics: control channels, node \
                                           configuration and commands, events, the metrics \
                                           summary, the runtime fleet and the global route \
                                           table. Served under `/api/platform`.")
    ),
    modifiers(&Security),
    paths(
        // Accounts and tokens.
        crate::api::identity::list_accounts,
        crate::api::identity::create_account,
        crate::api::identity::get_account,
        crate::api::identity::update_account,
        crate::api::identity::create_token,
        crate::api::identity::list_tokens,
        crate::api::identity::revoke_token,
        // The audit log.
        crate::api::audit::read_audit,
        // Applications.
        crate::api::catalog::apps::list_apps,
        crate::api::catalog::apps::create_app,
        crate::api::catalog::apps::get_app,
        crate::api::catalog::apps::delete_app,
        crate::api::catalog::apps::list_retired_apps,
        crate::api::catalog::apps::release_retired_app,
        crate::api::catalog::artifacts::list_artifacts,
        crate::api::catalog::artifacts::upload_artifact,
        crate::api::catalog::artifacts::get_artifact,
        crate::api::catalog::versions::upload_version_with_lineage,
        crate::api::catalog::versions::list_versions,
        crate::api::catalog::versions::upload_version,
        crate::api::catalog::versions::get_version,
        crate::api::catalog::environments::list_environments,
        crate::api::catalog::environments::create_environment,
        crate::api::catalog::environments::get_environment,
        crate::api::catalog::deployments::current_deployment,
        crate::api::catalog::deployments::list_deployments,
        crate::api::catalog::deployments::deploy,
        crate::api::catalog::deployments::deployment_plan_for_environment,
        crate::api::catalog::deployments::rollback,
        crate::api::catalog::deployments::deployment_plan,
        crate::api::catalog::providers::list_provider_target_grants,
        crate::api::catalog::providers::grant_provider_target,
        crate::api::catalog::providers::get_provider_target_grant,
        crate::api::catalog::providers::revoke_provider_target_grant,
        crate::api::catalog::providers::provider_target_grant_audit,
        crate::api::catalog::providers::list_provider_deployments,
        crate::api::catalog::providers::begin_provider_deployment,
        crate::api::catalog::providers::get_provider_deployment,
        crate::api::catalog::providers::cancel_provider_deployment,
        crate::api::catalog::providers::rollback_provider_deployment,
        crate::api::catalog::routes::list_routes,
        crate::api::catalog::routes::create_route,
        crate::api::catalog::routes::update_route,
        crate::api::catalog::routes::delete_route,
        crate::api::catalog::secrets::list_secrets,
        crate::api::catalog::secrets::put_secret,
        crate::api::catalog::secrets::delete_secret,
        crate::api::catalog::storage::storage_token,
        // Hostname grants.
        crate::api::catalog::domains::list_grants,
        crate::api::catalog::domains::create_grant,
        crate::api::catalog::domains::delete_grant,
        // Providers.
        crate::api::catalog::providers::list_credentials,
        crate::api::catalog::providers::provision_credential,
        crate::api::catalog::providers::get_credential,
        crate::api::catalog::providers::revoke_credential,
        crate::api::catalog::providers::rotate_credential,
        crate::api::catalog::providers::credential_audit,
        crate::api::catalog::providers::list_driver_releases,
        crate::api::catalog::providers::install_driver_release,
        crate::api::catalog::providers::get_driver_release,
        crate::api::catalog::providers::driver_release_audit,
        crate::api::catalog::providers::list_installations,
        crate::api::catalog::providers::register_installation,
        crate::api::catalog::providers::get_installation,
        crate::api::catalog::providers::delete_installation,
        crate::api::catalog::providers::disable_installation,
        crate::api::catalog::providers::enable_installation,
        crate::api::catalog::providers::installation_audit,
        crate::api::catalog::providers::list_installation_targets,
        crate::api::catalog::providers::register_target,
        crate::api::catalog::providers::list_targets,
        crate::api::catalog::providers::get_target,
        crate::api::catalog::providers::delete_target,
        crate::api::catalog::providers::disable_target,
        crate::api::catalog::providers::enable_target,
        crate::api::catalog::providers::target_audit,
        crate::api::catalog::providers::list_provider_target_grant_history,
        crate::api::catalog::providers::get_provider_target_grant_history,
        crate::api::catalog::providers::provider_target_grant_history_audit,
        // Identity migrations and long operations.
        crate::api::migrations::get_migration,
        crate::api::migrations::start_migration,
        crate::api::migrations::get_operation,
        crate::api::migrations::cancel_operation,
        // Backend restores.
        crate::api::restores::get_installation,
        crate::api::restores::start_restore,
        crate::api::restores::get_restore,
        // Platform: control channels.
        crate::api::nodes::list_sessions,
        crate::api::nodes::get_session,
        crate::api::nodes::get_heartbeats,
        // Platform: node configuration.
        crate::api::configs::list_all_configs,
        crate::api::configs::get_config_stats,
        crate::api::configs::get_node_configs,
        crate::api::configs::update_node_configs,
        crate::api::configs::delete_node_configs,
        crate::api::configs::get_config_history,
        crate::api::configs::get_config_value,
        crate::api::configs::delete_config,
        crate::api::configs::get_config_key_history,
        crate::api::configs::rollback_config_key,
        crate::api::configs::push_all_configs,
        crate::api::configs::push_config,
        // Platform: commands.
        crate::api::commands::list_commands,
        crate::api::commands::execute_command,
        crate::api::commands::list_node_commands,
        crate::api::commands::get_command_status,
        crate::api::commands::cancel_command,
        // Platform: events and metrics.
        crate::api::events::list_events,
        crate::api::events::get_event_stats,
        crate::api::metrics::get_metrics_summary,
        // Platform: the runtime fleet.
        crate::api::apps::list_fleet,
        crate::api::apps::node_status,
        crate::api::apps::unload_app,
        // Platform: the global route table.
        crate::api::catalog::routes::list_all_routes,
    ),
    components(
        schemas(
            ErrorBody,
            ProductOperationDocument,
            BackendRestoreSetDocument,
            BackendRestoreExecutionReportDocument,
            RestoreDigestDocument,
            ArtifactVersionRefDocument,
            ProviderNeutralPlanDocument,
            CompatibilityProfileDocument,
            CanonicalArtifactDocument,
            ProviderDriverManifestDocument,
            BinaryBody,
        ),
        responses(
            BadRequest,
            Unauthenticated,
            Forbidden,
            NotFound,
            Conflict,
            PreconditionRequired,
            PreconditionFailed,
            PayloadTooLarge,
            UnprocessableEntity,
            Internal,
            BadGateway,
            ServiceUnavailable,
        )
    )
)]
pub struct ControlApi;

/// The rendered document.
pub fn document() -> utoipa::openapi::OpenApi {
    ControlApi::openapi()
}

/// The document as the committed file holds it: pretty JSON with every
/// object's keys sorted, so two renders from two builds compare byte for
/// byte whatever map type `serde_json` was compiled with.
pub fn render() -> String {
    let value = serde_json::to_value(document()).expect("the OpenAPI document serialises");
    let mut text = serde_json::to_string_pretty(&sorted(value)).expect("a JSON value renders");
    text.push('\n');
    text
}

/// The same value with every object's keys in lexical order, recursively.
pub fn sorted(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = serde_json::Map::new();
            for (key, inner) in entries {
                out.insert(key, sorted(inner));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted).collect())
        }
        other => other,
    }
}

/// The two ways a caller presents a credential to the authorization gate,
/// either of which satisfies every operation. When `api_auth_enabled` is
/// off the gate resolves every caller to an anonymous principal and no
/// credential is needed; the document describes the enforced contract.
struct Security;

impl Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_default();
        components.add_security_scheme(
            SECURITY_BEARER,
            SecurityScheme::Http(
                Http::builder()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "`Authorization: Bearer <token>` with an API token minted by \
                         `POST /api/v1/accounts/{name}/tokens` or an `api_keys` entry of the \
                         configuration file.",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            SECURITY_API_KEY,
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "X-API-Key",
                "The same credential as the bearer form, in the `X-API-Key` header.",
            ))),
        );
        openapi.security = Some(vec![
            SecurityRequirement::new::<_, _, &str>(SECURITY_BEARER, []),
            SecurityRequirement::new::<_, _, &str>(SECURITY_API_KEY, []),
        ]);
    }
}

// ---------------------------------------------------------------------------
// The error envelope.
//
// `api::error` renders the envelope by hand from `ErrorDetail`, which also
// carries the legacy body and the status; these types describe only the
// wire shape of `/api/v1` and `/api/platform` answers, and the test below
// holds them to `contracts/v1/control-error.schema.json`.
// ---------------------------------------------------------------------------

/// The structured error answer of `/api/v1` and `/api/platform`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorEnvelope,
}

/// The one error object. `code` is the closed set in
/// `contracts/v1/control-error.schema.json`; `requestId` is the id echoed in
/// the `x-request-id` header of the same answer. The optional members are
/// absent rather than `null` when they do not apply, as the contract
/// requires.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEnvelope {
    pub code: ErrorCode,
    /// Human-readable, not stable: clients branch on `code`. On `internal`,
    /// `database-unavailable` and a `backend-unavailable` raised by a
    /// component the control plane dialled, this is a fixed sentence: what
    /// the component said is in the server log under `requestId`.
    #[schema(min_length = 1, max_length = 4096)]
    pub message: String,
    /// The request this answer belongs to, a canonical UUIDv7.
    #[schema(format = Uuid)]
    pub request_id: String,
    /// What the error is about, when one resource or field can be named.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub target: Option<ErrorTarget>,
    /// Field-level findings of an `invalid-argument`; always present, may
    /// be empty.
    #[schema(max_items = 100)]
    pub violations: Vec<ErrorViolation>,
    /// Whether the identical request may succeed later. A property of the
    /// code: `true` for exactly the codes `ErrorCode` names as retryable.
    pub retryable: bool,
    /// How long to wait before retrying, in seconds; only ever on a
    /// retryable code.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, maximum = 86400)]
    pub retry_after_seconds: Option<u64>,
    /// Where the rule that refused the request is written down: an absolute
    /// HTTPS link.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(
        nullable = false,
        format = "uri",
        pattern = "^https://",
        max_length = 2048
    )]
    pub docs: Option<String>,
    /// A provider driver's own diagnostic, when the failure came from a
    /// provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub provider: Option<ProviderDiagnostic>,
}

/// The closed object the contract allows under `provider`: the
/// `driverError` vocabulary of `contracts/v1/provider-driver-process.schema.json`
/// and nothing else. Rendered by hand from the driver crate's own enums so
/// the category and side-effect sets cannot drift from what a driver says.
impl PartialSchema for ProviderDiagnostic {
    fn schema() -> RefOr<Schema> {
        fn kebab_values<T: Serialize>(values: &[T]) -> Vec<String> {
            values
                .iter()
                .map(|value| {
                    serde_json::to_value(value)
                        .expect("a unit variant serialises")
                        .as_str()
                        .expect("a unit variant is a string")
                        .to_string()
                })
                .collect()
        }
        let categories = kebab_values(&[
            DriverErrorCategory::Authentication,
            DriverErrorCategory::Permission,
            DriverErrorCategory::Quota,
            DriverErrorCategory::Configuration,
            DriverErrorCategory::Build,
            DriverErrorCategory::Upload,
            DriverErrorCategory::Resource,
            DriverErrorCategory::Verification,
            DriverErrorCategory::Activation,
            DriverErrorCategory::TransientProvider,
            DriverErrorCategory::UnknownState,
            DriverErrorCategory::DriverIntegrity,
            DriverErrorCategory::Internal,
        ]);
        let side_effects = kebab_values(&[
            SideEffect::None,
            SideEffect::Possible,
            SideEffect::Confirmed,
        ]);
        ObjectBuilder::new()
            .description(Some(
                "Provider-side diagnostic passed through from a driver outcome, in the \
                 driver's closed error vocabulary. Never carries provider management \
                 credentials.",
            ))
            .property(
                "backendId",
                string_property("The backend the driver drives, e.g. `cloudflare-workers-pages`."),
            )
            .required("backendId")
            .property(
                "category",
                ObjectBuilder::new()
                    .schema_type(SchemaType::Type(Type::String))
                    .enum_values(Some(categories)),
            )
            .required("category")
            .property(
                "code",
                ObjectBuilder::new()
                    .schema_type(SchemaType::Type(Type::String))
                    .pattern(Some("^[A-Z][A-Z0-9_]{1,127}$"))
                    .description(Some("The driver's own failure class.")),
            )
            .required("code")
            .property(
                "sideEffect",
                ObjectBuilder::new()
                    .schema_type(SchemaType::Type(Type::String))
                    .enum_values(Some(side_effects)),
            )
            .required("sideEffect")
            .property(
                "requestId",
                ObjectBuilder::new()
                    .schema_type(SchemaType::Type(Type::String))
                    .min_length(Some(1))
                    .max_length(Some(512))
                    .description(Some(
                        "The provider's own request id, when the driver was given one.",
                    )),
            )
            .additional_properties(Some(
                utoipa::openapi::schema::AdditionalProperties::FreeForm(false),
            ))
            .into()
    }
}

impl ToSchema for ProviderDiagnostic {}

/// The resource, and optionally the field, an error is about.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorTarget {
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// One field-level finding.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorViolation {
    pub field: String,
    /// A kebab-case class of the finding, finer than the envelope's `code`.
    pub code: String,
    pub message: String,
}

/// The closed set of error codes. Rendered from `codes::ALL` so the document
/// cannot miss a code the server produces, and lists exactly one it does
/// not: `offset-out-of-range`, kept because a published code is never
/// withdrawn from a closed enum.
#[derive(Debug, Serialize)]
pub struct ErrorCode(pub &'static str);

impl PartialSchema for ErrorCode {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(SchemaType::Type(Type::String))
            .description(Some(
                "Stable machine-readable error class; `contracts/v1/control-error.schema.json` \
                 is the authority. `service-unavailable`, `backend-unavailable`, \
                 `database-unavailable` and `idempotency-request-in-progress` are the \
                 retryable ones; `retryable` in the envelope is `true` for exactly those. \
                 `offset-out-of-range` is reserved and emitted by nothing: no route takes \
                 an offset, and the token stays in the enum for clients that already \
                 branch on it.",
            ))
            .enum_values(Some(codes::ALL.iter().copied()))
            .into()
    }
}

impl ToSchema for ErrorCode {}

macro_rules! error_response {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(ToResponse)]
        #[response(description = $description)]
        pub struct $name(pub ErrorBody);
    };
}

error_response!(
    BadRequest,
    "The request cannot be understood or fails validation: a malformed body or query, a \
     `limit` outside `1..=1000`, or a cursor that was not issued for this collection and \
     filter (`invalid-argument`, `limit-out-of-range`, `cursor-invalid`, \
     `idempotency-key-required`)."
);
error_response!(
    NotFound,
    "No resource at this path (`not-found`), or no route at all (`route-not-found`)."
);
error_response!(
    Conflict,
    "The request contradicts the current state: the name is taken (`already-exists`), the \
     resource is in use, an idempotency key was reused with a different request, or a \
     dependency the call needs is deliberately not configured on this control plane \
     (`conflict`, `revision-conflict`, `idempotency-key-reused`, \
     `idempotency-request-in-progress`, `not-configured`, the `provider-*` lifecycle codes)."
);
error_response!(
    PreconditionRequired,
    "The mutation needs an `If-Match` revision or an `Idempotency-Key` header and none was \
     sent (`precondition-required`)."
);
error_response!(
    PreconditionFailed,
    "The `If-Match` revision does not match the resource's current revision \
     (`revision-conflict`)."
);
error_response!(
    PayloadTooLarge,
    "The body exceeds the route's limit (`payload-too-large`)."
);
error_response!(
    UnprocessableEntity,
    "The body is well-formed but cannot be acted on: a bundle or envelope that fails \
     verification, a restore set that fails validation (`invalid-argument`)."
);
error_response!(
    Internal,
    "The control plane failed on its own side (`internal`, `database-unavailable`)."
);
error_response!(
    BadGateway,
    "A node or backend the control plane had to reach answered with an error \
     (`backend-unavailable`)."
);
error_response!(
    ServiceUnavailable,
    "A component the request needs is not reachable: the secret store, the migration \
     coordinator, a node's channel (`service-unavailable`, `backend-unavailable`). \
     Retryable."
);
error_response!(
    Unauthenticated,
    "No credential was presented, or the credential is unknown or revoked \
     (`unauthenticated`). Answered by the authorization gate in front of every route; \
     the message never says which of those it was."
);
error_response!(
    Forbidden,
    "The credential is known but its role or scope does not cover this operation on this \
     application and environment (`insufficient-scope`), or the call is refused for \
     another reason (`forbidden`). Answered by the authorization gate in front of every \
     route, or by the route itself."
);

// ---------------------------------------------------------------------------
// Documents shared with other crates.
//
// Each marker names the JSON Schema that is the authority for the type, and
// gives enough of the shape for a generated client to hold the value. The
// Rust types live in `deis-wire`, `deis-provider-driver` and `deis-bundle`,
// which do not describe themselves in OpenAPI terms.
// ---------------------------------------------------------------------------

fn contract_object(name: &str, schema_file: &str, detail: &str) -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::Object))
        .description(Some(format!(
            "{name}, as `contracts/v1/{schema_file}` defines it. {detail}"
        )))
}

fn string_property(description: &str) -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::String))
        .description(Some(description))
}

fn uuid_property(description: &str) -> ObjectBuilder {
    string_property(description).format(Some(SchemaFormat::KnownFormat(KnownFormat::Uuid)))
}

fn date_time_property(description: &str) -> ObjectBuilder {
    string_property(description).format(Some(SchemaFormat::KnownFormat(KnownFormat::DateTime)))
}

fn integer_property(description: &str) -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(SchemaType::Type(Type::Integer))
        .description(Some(description))
}

/// A long operation: the answer of every 202 and the resource behind
/// `GET /api/v1/operations/{operation_id}`.
pub struct ProductOperationDocument;

impl PartialSchema for ProductOperationDocument {
    fn schema() -> RefOr<Schema> {
        contract_object(
            "A product operation",
            "operation.schema.json",
            "The durable record of a mutation that takes time. `state` walks the twelve \
             transitions of `contracts/v1/operation-transitions.json`; `revision` grows \
             with every change and is the `ETag`; `error` is set exactly in `failed`.",
        )
        .property(
            "$schema",
            string_property("The `$id` of the schema the record conforms to."),
        )
        .required("$schema")
        .property("schemaVersion", integer_property("Always 1."))
        .required("schemaVersion")
        .property("operationId", uuid_property("Canonical UUIDv7 of the operation."))
        .required("operationId")
        .property(
            "kind",
            string_property("What the operation does: `identity-migration`, `backend-restore`, `provider-deployment`."),
        )
        .required("kind")
        .property(
            "scope",
            ObjectBuilder::new()
                .schema_type(SchemaType::Type(Type::Object))
                .description(Some("The organization, application and environment the operation acts in."))
                .property("organizationId", uuid_property("Canonical UUIDv7."))
                .required("organizationId")
                .property("applicationId", uuid_property("Canonical UUIDv7, when scoped to one application."))
                .property("environmentId", uuid_property("Canonical UUIDv7, when scoped to one environment.")),
        )
        .required("scope")
        .property(
            "target",
            ObjectBuilder::new()
                .schema_type(SchemaType::Type(Type::Object))
                .description(Some("The resource the operation mutates and the revision it expects."))
                .property("kind", string_property("The resource kind."))
                .required("kind")
                .property("id", uuid_property("Canonical UUIDv7 of the resource."))
                .required("id")
                .property("expectedRevision", integer_property("The revision the operation was started against."))
                .required("expectedRevision"),
        )
        .required("target")
        .property(
            "state",
            string_property("`pending`, `running`, `waiting`, `succeeded`, `failed`, `cancelling` or `cancelled`.")
                .enum_values(Some([
                    "pending",
                    "running",
                    "waiting",
                    "succeeded",
                    "failed",
                    "cancelling",
                    "cancelled",
                ])),
        )
        .required("state")
        .property("phase", string_property("The kind-specific step the operation is in."))
        .required("phase")
        .property("revision", integer_property("Grows with every change; the `ETag` of the record."))
        .required("revision")
        .property(
            "idempotencyKeyDigest",
            ObjectBuilder::new()
                .schema_type(SchemaType::Type(Type::Object))
                .description(Some("SHA-256 of the `Idempotency-Key` that started the operation."))
                .property("algorithm", string_property("`sha256`."))
                .required("algorithm")
                .property("value", string_property("64 lowercase hexadecimal digits."))
                .required("value"),
        )
        .required("idempotencyKeyDigest")
        .property("actorId", uuid_property("Who started it."))
        .required("actorId")
        .property("requestId", uuid_property("The request that started it."))
        .required("requestId")
        .property("correlationId", uuid_property("The correlation id that started it."))
        .required("correlationId")
        .property("progress", integer_property("0..=100 when the kind reports progress."))
        .property("cancellable", ObjectBuilder::new().schema_type(SchemaType::Type(Type::Boolean)))
        .required("cancellable")
        .property("retryable", ObjectBuilder::new().schema_type(SchemaType::Type(Type::Boolean)))
        .required("retryable")
        .property(
            "childOperationIds",
            ArrayBuilder::new().items(uuid_property("Canonical UUIDv7 of a child operation.")),
        )
        .required("childOperationIds")
        .property(
            "providerOperations",
            ArrayBuilder::new().items(
                ObjectBuilder::new()
                    .schema_type(SchemaType::Type(Type::Object))
                    .description(Some("One provider driver operation this operation drove; see `providerOperations` in the contract.")),
            ),
        )
        .required("providerOperations")
        .property(
            "error",
            ObjectBuilder::new()
                .schema_type(SchemaType::Type(Type::Object))
                .description(Some("Why the operation failed; present exactly when `state` is `failed`."))
                .property("code", string_property("A stable error class."))
                .required("code")
                .property("message", string_property("Human-readable detail."))
                .required("message")
                .property("retryable", ObjectBuilder::new().schema_type(SchemaType::Type(Type::Boolean)))
                .required("retryable")
                .property("retryAfterSeconds", integer_property("How long to wait before retrying."))
                .property("remediation", string_property("What an operator can do about it.")),
        )
        .property("createdAt", date_time_property("RFC 3339."))
        .required("createdAt")
        .property("startedAt", date_time_property("RFC 3339, once running."))
        .property("updatedAt", date_time_property("RFC 3339."))
        .required("updatedAt")
        .property("finishedAt", date_time_property("RFC 3339, once terminal."))
        .into()
    }
}

impl ToSchema for ProductOperationDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("ProductOperation")
    }
}

/// The restore set a backend restore is started from.
pub struct BackendRestoreSetDocument;

impl PartialSchema for BackendRestoreSetDocument {
    fn schema() -> RefOr<Schema> {
        contract_object(
            "A backend restore set",
            "backend-restore-set.schema.json",
            "The consistent snapshot set of every storage backend, taken behind one \
             consistency barrier. Validated on `POST /api/v1/backend-restores`; a set that \
             fails validation is refused with 422.",
        )
        .property(
            "$schema",
            string_property("The `$id` of the schema the set conforms to."),
        )
        .required("$schema")
        .property("schemaVersion", integer_property("Always 1."))
        .required("schemaVersion")
        .property(
            "restoreSetId",
            uuid_property("Canonical UUIDv7 of the set."),
        )
        .required("restoreSetId")
        .property(
            "scope",
            ObjectBuilder::new().schema_type(SchemaType::Type(Type::Object)),
        )
        .required("scope")
        .property(
            "consistencyBarrier",
            ObjectBuilder::new().schema_type(SchemaType::Type(Type::Object)),
        )
        .required("consistencyBarrier")
        .property(
            "topologyDriver",
            ObjectBuilder::new().schema_type(SchemaType::Type(Type::Object)),
        )
        .required("topologyDriver")
        .property(
            "components",
            ArrayBuilder::new()
                .items(ObjectBuilder::new().schema_type(SchemaType::Type(Type::Object))),
        )
        .required("components")
        .property("createdAt", date_time_property("RFC 3339."))
        .required("createdAt")
        .into()
    }
}

impl ToSchema for BackendRestoreSetDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("BackendRestoreSet")
    }
}

/// What the restore driver reported when the restore finished.
pub struct BackendRestoreExecutionReportDocument;

impl PartialSchema for BackendRestoreExecutionReportDocument {
    fn schema() -> RefOr<Schema> {
        contract_object(
            "A backend restore execution report",
            "backend-restore-report.schema.json",
            "Present on a restore plan once the topology driver has finished; `state` is \
             `succeeded` or `rolled-back`.",
        )
        .property(
            "$schema",
            string_property("The `$id` of the schema the report conforms to."),
        )
        .required("$schema")
        .property("schemaVersion", integer_property("Always 1."))
        .required("schemaVersion")
        .property("operationId", uuid_property("Canonical UUIDv7."))
        .required("operationId")
        .property("restoreSetId", uuid_property("Canonical UUIDv7."))
        .required("restoreSetId")
        .property("restoreSetSha256", RestoreDigestDocument::schema())
        .required("restoreSetSha256")
        .property(
            "state",
            string_property("`succeeded` or `rolled-back`.")
                .enum_values(Some(["succeeded", "rolled-back"])),
        )
        .required("state")
        .property(
            "components",
            ArrayBuilder::new()
                .items(ObjectBuilder::new().schema_type(SchemaType::Type(Type::Object))),
        )
        .required("components")
        .into()
    }
}

impl ToSchema for BackendRestoreExecutionReportDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("BackendRestoreExecutionReport")
    }
}

/// An algorithm-tagged digest as the restore contracts spell it.
pub struct RestoreDigestDocument;

impl PartialSchema for RestoreDigestDocument {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(SchemaType::Type(Type::Object))
            .description(Some("An algorithm-tagged digest."))
            .property("algorithm", string_property("`sha256`."))
            .required("algorithm")
            .property("value", string_property("64 lowercase hexadecimal digits."))
            .required("value")
            .into()
    }
}

impl ToSchema for RestoreDigestDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("RestoreDigest")
    }
}

/// Which canonical artifact version a provider deployment ships.
pub struct ArtifactVersionRefDocument;

impl PartialSchema for ArtifactVersionRefDocument {
    fn schema() -> RefOr<Schema> {
        contract_object(
            "An artifact version reference",
            "provider-driver-process.schema.json",
            "Names the canonical artifact by id and pins both digests the driver must see.",
        )
        .property(
            "artifactVersionId",
            uuid_property("Canonical UUIDv7 of the version."),
        )
        .required("artifactVersionId")
        .property(
            "canonicalArtifactSha256",
            string_property("64 lowercase hexadecimal digits."),
        )
        .required("canonicalArtifactSha256")
        .property(
            "providerPayloadSha256",
            string_property("64 lowercase hexadecimal digits."),
        )
        .required("providerPayloadSha256")
        .into()
    }
}

impl ToSchema for ArtifactVersionRefDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("ArtifactVersionRef")
    }
}

/// The provider-neutral plan digests a deployment carries.
pub struct ProviderNeutralPlanDocument;

impl PartialSchema for ProviderNeutralPlanDocument {
    fn schema() -> RefOr<Schema> {
        contract_object(
            "A provider-neutral plan",
            "provider-driver-process.schema.json",
            "Every member is optional; each is a 64-digit lowercase SHA-256, except \
             `priorDeploymentId`, which is a canonical UUIDv7.",
        )
        .property(
            "bindingSnapshotDigest",
            string_property("SHA-256 of the binding snapshot."),
        )
        .property(
            "routePlanDigest",
            string_property("SHA-256 of the route plan."),
        )
        .property(
            "triggerPlanDigest",
            string_property("SHA-256 of the trigger plan."),
        )
        .property(
            "trafficPolicyDigest",
            string_property("SHA-256 of the traffic policy."),
        )
        .property(
            "priorDeploymentId",
            uuid_property("The deployment this one supersedes."),
        )
        .into()
    }
}

impl ToSchema for ProviderNeutralPlanDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("ProviderNeutralPlan")
    }
}

/// A version's declared compatibility, as the bundle manifest spells it.
pub struct CompatibilityProfileDocument;

impl PartialSchema for CompatibilityProfileDocument {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(SchemaType::Type(Type::Object))
            .description(Some(
                "The compatibility a version declared, resolved: `date` is the compatibility \
                 date (`YYYY-MM-DD`) when one was declared, `flags` the runtime flags that \
                 are on for it.",
            ))
            .property("date", string_property("`YYYY-MM-DD`, when declared."))
            .property(
                "flags",
                ArrayBuilder::new().items(string_property("A runtime compatibility flag name.")),
            )
            .into()
    }
}

impl ToSchema for CompatibilityProfileDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("CompatibilityProfile")
    }
}

/// A raw request body: bundle bytes, an artifact archive, a provider
/// derivation envelope.
#[derive(ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct BinaryBody(pub Vec<u8>);

/// A canonical artifact record, the answer of the artifact routes.
pub struct CanonicalArtifactDocument;

impl PartialSchema for CanonicalArtifactDocument {
    fn schema() -> RefOr<Schema> {
        contract_object(
            "A canonical artifact record",
            "canonical-artifact.schema.json",
            "The verified provider-neutral Build Output of one upload: its content root \
             digest, standard version and file inventory. Returned as stored.",
        )
        .property(
            "$schema",
            string_property("The `$id` of the schema the record conforms to."),
        )
        .property("schemaVersion", integer_property("Always 1."))
        .property(
            "artifactId",
            uuid_property("Canonical UUIDv7 of the artifact."),
        )
        .property(
            "artifactSha256",
            string_property("64 lowercase hexadecimal digits."),
        )
        .property(
            "contentRootSha256",
            string_property("64 lowercase hexadecimal digits."),
        )
        .property(
            "standardVersion",
            string_property("The Build Output standard version."),
        )
        .into()
    }
}

impl ToSchema for CanonicalArtifactDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("CanonicalArtifact")
    }
}

/// A provider driver manifest, as a target registration carries it.
pub struct ProviderDriverManifestDocument;

impl PartialSchema for ProviderDriverManifestDocument {
    fn schema() -> RefOr<Schema> {
        contract_object(
            "A provider driver manifest",
            "provider-driver.schema.json",
            "Validated against the contract and against the installed driver release before \
             a target is registered.",
        )
        .into()
    }
}

impl ToSchema for ProviderDriverManifestDocument {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("ProviderDriverManifest")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::*;

    fn rendered() -> Value {
        serde_json::from_str(&render()).unwrap()
    }

    fn paths(document: &Value) -> Vec<(String, String, &Value)> {
        let mut out = Vec::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                out.push((path.clone(), method.clone(), operation));
            }
        }
        out
    }

    /// Walk every schema object in `components.schemas` and yield
    /// `(schema name, property name, property schema)`.
    fn properties(document: &Value) -> Vec<(String, String, &Value)> {
        fn walk<'a>(name: &str, schema: &'a Value, out: &mut Vec<(String, String, &'a Value)>) {
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (property, inner) in props {
                    out.push((name.to_string(), property.clone(), inner));
                    walk(name, inner, out);
                }
            }
            for key in ["allOf", "oneOf", "anyOf"] {
                if let Some(list) = schema.get(key).and_then(Value::as_array) {
                    for inner in list {
                        walk(name, inner, out);
                    }
                }
            }
            if let Some(items) = schema.get("items") {
                walk(name, items, out);
            }
        }
        let mut out = Vec::new();
        for (name, schema) in document["components"]["schemas"].as_object().unwrap() {
            walk(name, schema, &mut out);
        }
        out
    }

    #[test]
    fn the_document_names_its_version_and_both_tags() {
        let document = rendered();
        assert_eq!(document["openapi"], "3.1.0");
        assert_eq!(document["info"]["version"], DOCUMENT_VERSION);
        let tags: BTreeSet<&str> = document["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| tag["name"].as_str().unwrap())
            .collect();
        assert_eq!(tags, BTreeSet::from([TAG_PRODUCT, TAG_PLATFORM]));
        assert!(document["servers"].as_array().unwrap().len() == 1);
        let schemes = document["components"]["securitySchemes"]
            .as_object()
            .unwrap();
        assert!(schemes.contains_key(SECURITY_BEARER));
        assert!(schemes.contains_key(SECURITY_API_KEY));
    }

    #[test]
    fn every_operation_has_an_id_a_tag_and_an_error_response() {
        let document = rendered();
        let mut ids = BTreeSet::new();
        for (path, method, operation) in paths(&document) {
            let id = operation["operationId"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} {path} has no operationId"));
            assert!(
                ids.insert(id.to_string()),
                "operationId {id} is used twice ({method} {path})"
            );
            let tags = operation["tags"].as_array().unwrap();
            assert_eq!(tags.len(), 1, "{method} {path} must carry exactly one tag");
            let tag = tags[0].as_str().unwrap();
            assert!(
                tag == TAG_PRODUCT || tag == TAG_PLATFORM,
                "{method} {path} carries unknown tag {tag}"
            );
            let responses = operation["responses"].as_object().unwrap();
            assert!(
                responses
                    .keys()
                    .any(|status| status.starts_with('4') || status.starts_with('5')),
                "{method} {path} documents no error response"
            );
            assert!(
                responses.keys().any(|status| status.starts_with('2')),
                "{method} {path} documents no success response"
            );
        }
    }

    #[test]
    fn the_tag_decides_the_prefix_and_the_node_channel_is_never_listed() {
        let document = rendered();
        for (path, method, operation) in paths(&document) {
            let tag = operation["tags"][0].as_str().unwrap();
            let expected = if tag == TAG_PRODUCT {
                "/api/v1/"
            } else {
                "/api/platform/"
            };
            assert!(
                path.starts_with(expected),
                "{method} {path} is tagged {tag} but does not live under {expected}"
            );
            assert!(!path.starts_with("/api/nodes"), "{path} is a node route");
            assert!(
                !path.starts_with("/api/routing-table"),
                "{path} is a node route"
            );
            assert!(!path.ends_with("/bundle"), "{path} is the bundle download");
            assert_ne!(
                path, "/api/metrics",
                "the Prometheus exposition is not versioned"
            );
        }
    }

    #[test]
    fn every_list_answers_in_the_envelope() {
        let document = rendered();
        for (path, method, operation) in paths(&document) {
            if method != "get" {
                continue;
            }
            let Some(parameters) = operation["parameters"].as_array() else {
                continue;
            };
            let has_cursor = parameters
                .iter()
                .any(|parameter| parameter["name"] == "cursor" && parameter["in"] == "query");
            if !has_cursor {
                continue;
            }
            let body = &operation["responses"]["200"]["content"]["application/json"]["schema"];
            let reference = body["$ref"].as_str().unwrap_or_else(|| {
                panic!("{method} {path} takes a cursor but its 200 is not a named schema")
            });
            assert!(
                reference.starts_with("#/components/schemas/ControlListPage_"),
                "{method} {path} takes a cursor but answers {reference}"
            );
        }
    }

    #[test]
    fn the_error_code_set_is_the_contract_set() {
        let document = rendered();
        let listed: BTreeSet<&str> = document["components"]["schemas"]["ErrorCode"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|code| code.as_str().unwrap())
            .collect();
        assert_eq!(listed, codes::ALL.iter().copied().collect::<BTreeSet<_>>());

        let contract_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../contracts/v1/control-error.schema.json");
        let contract: Value =
            serde_json::from_str(&std::fs::read_to_string(&contract_path).unwrap()).unwrap();
        let contract_codes: BTreeSet<&str> = contract["properties"]["error"]["properties"]["code"]
            ["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("{} lists no code enum", contract_path.display()))
            .iter()
            .map(|code| code.as_str().unwrap())
            .collect();
        assert_eq!(
            listed, contract_codes,
            "the OpenAPI error codes and contracts/v1/control-error.schema.json disagree"
        );
        // The envelope's members, as the contract names them.
        let members: BTreeSet<&str> =
            document["components"]["schemas"]["ErrorEnvelope"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
        let contract_members: BTreeSet<&str> = contract["properties"]["error"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(members, contract_members);
        let required: BTreeSet<&str> =
            document["components"]["schemas"]["ErrorEnvelope"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|member| member.as_str().unwrap())
                .collect();
        let contract_required: BTreeSet<&str> = contract["properties"]["error"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| member.as_str().unwrap())
            .collect();
        assert_eq!(required, contract_required);
        // The optional members are absent, never null.
        let envelope = &document["components"]["schemas"]["ErrorEnvelope"]["properties"];
        for member in ["target", "retryAfterSeconds", "docs", "provider"] {
            let rendered = serde_json::to_string(&envelope[member]).unwrap();
            assert!(!rendered.contains("null"), "{member} is rendered nullable");
        }
        assert_eq!(envelope["retryAfterSeconds"]["maximum"], 86400);
        assert_eq!(envelope["docs"]["format"], "uri");

        // `provider` is the contract's closed object, member for member and
        // enum for enum.
        let provider = &document["components"]["schemas"]["ProviderDiagnostic"];
        let contract_provider = &contract["properties"]["error"]["properties"]["provider"];
        assert_eq!(provider["additionalProperties"], false);
        let keys = |value: &Value| -> BTreeSet<String> {
            value.as_object().unwrap().keys().cloned().collect()
        };
        assert_eq!(
            keys(&provider["properties"]),
            keys(&contract_provider["properties"])
        );
        for member in ["category", "sideEffect"] {
            assert_eq!(
                provider["properties"][member]["enum"],
                contract_provider["properties"][member]["enum"],
                "{member}"
            );
        }
        assert_eq!(
            provider["properties"]["code"]["pattern"],
            contract_provider["properties"]["code"]["pattern"]
        );
        let contract_backends: BTreeSet<&str> =
            contract_provider["properties"]["backendId"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
        assert!(
            contract_provider["required"]
                .as_array()
                .unwrap()
                .iter()
                .all(|member| provider["required"].as_array().unwrap().contains(member)),
            "every member the contract requires is required here"
        );
        assert!(
            !contract_backends.is_empty(),
            "the contract enumerates the backends; the server takes the registry's string"
        );
    }

    /// Schemas that legitimately carry a member named like a secret: the
    /// metadata of a secret (its name and revision, never its value), the
    /// two answers that hand a freshly minted credential to the person who
    /// asked for it, and the plan and deletion records that count secrets
    /// or name their revisions. Kept in step with
    /// `scripts/check-openapi-compat.py`.
    const SECRET_NAME_ALLOWLIST: &[(&str, &str)] = &[
        ("SecretMeta", "name"),
        ("SecretChangeResponse", "secret"),
        ("MintResponse", "secret"),
        ("StorageTokenResponse", "token"),
        ("ControlListPage_SecretMeta", "items"),
        // A count of secrets removed with an application.
        ("DeletedApp", "secrets"),
        // Secret *revision heads* a plan was cut against, and whether they
        // are compared: names and revision ids, never values.
        ("DeploymentPlan", "expected_secret_heads"),
        ("DeploymentPlan", "compare_secret_heads"),
    ];

    fn looks_like_a_secret_name(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        let flat = lower.replace(['_', '-'], "");
        lower.contains("secret")
            || lower.contains("password")
            || flat.contains("privatekey")
            || flat.contains("masterkey")
            || lower.contains("preshared")
            || lower == "psk"
            || lower.ends_with("_psk")
            || lower.starts_with("psk_")
    }

    #[test]
    fn no_schema_property_is_named_like_a_secret_outside_the_allowlist() {
        let document = rendered();
        let mut offenders = Vec::new();
        for (schema, property, _) in properties(&document) {
            if looks_like_a_secret_name(&property)
                && !SECRET_NAME_ALLOWLIST.contains(&(schema.as_str(), property.as_str()))
            {
                offenders.push(format!("{schema}.{property}"));
            }
        }
        assert!(
            offenders.is_empty(),
            "properties named like a credential outside the reviewed allowlist: {offenders:?}"
        );
        // The allowlist must not go stale either.
        let present: BTreeSet<(String, String)> = properties(&document)
            .into_iter()
            .map(|(schema, property, _)| (schema, property))
            .collect();
        for (schema, property) in SECRET_NAME_ALLOWLIST {
            assert!(
                present.contains(&(schema.to_string(), property.to_string())),
                "allowlist entry {schema}.{property} names nothing in the document"
            );
        }
    }

    fn looks_like_a_credential_value(value: &Value) -> bool {
        match value {
            Value::String(text) => {
                let lower = text.to_ascii_lowercase();
                lower.starts_with("deis_")
                    || lower.starts_with("sk_")
                    || lower.starts_with("-----begin")
                    || lower.starts_with("bearer ")
                    || (text.len() >= 32
                        && text
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        && text.chars().any(|c| c.is_ascii_digit())
                        && text.chars().any(|c| c.is_ascii_alphabetic())
                        && !text.contains('-'))
            }
            Value::Array(items) => items.iter().any(looks_like_a_credential_value),
            Value::Object(map) => map.values().any(looks_like_a_credential_value),
            _ => false,
        }
    }

    #[test]
    fn no_example_or_default_looks_like_a_credential() {
        let document = rendered();
        fn walk(path: &str, value: &Value, offenders: &mut Vec<String>) {
            if let Some(map) = value.as_object() {
                for key in ["example", "examples", "default"] {
                    if let Some(candidate) = map.get(key)
                        && looks_like_a_credential_value(candidate)
                    {
                        offenders.push(format!("{path}.{key}"));
                    }
                }
                for (key, inner) in map {
                    walk(&format!("{path}.{key}"), inner, offenders);
                }
            } else if let Some(items) = value.as_array() {
                for (index, inner) in items.iter().enumerate() {
                    walk(&format!("{path}[{index}]"), inner, offenders);
                }
            }
        }
        let mut offenders = Vec::new();
        walk("$", &document, &mut offenders);
        assert!(
            offenders.is_empty(),
            "credential-shaped example values: {offenders:?}"
        );
    }

    /// A call from the machine the control plane runs on, which is what the
    /// gate admits without a credential while nothing is enforced.
    async fn send(
        router: &axum::Router,
        method: &str,
        uri: &str,
        body: axum::body::Body,
        content_type: &str,
    ) -> (axum::http::StatusCode, Value) {
        use axum::extract::ConnectInfo;
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", content_type)
            .body(body)
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn send_json(
        router: &axum::Router,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (axum::http::StatusCode, Value) {
        let body = if body.is_null() {
            axum::body::Body::empty()
        } else {
            axum::body::Body::from(body.to_string())
        };
        send(router, method, uri, body, "application/json").await
    }

    /// The document is opt-in, so what it lists must be what the routers
    /// serve. Probe the real router: a path that is mounted answers an
    /// unknown method with 405, an unmounted one falls through to the
    /// 404 fallback; and each documented method must not be the one that
    /// gets the 405.
    #[tokio::test]
    async fn every_described_operation_is_mounted_under_its_prefix() {
        use axum::http::StatusCode;

        let document = rendered();
        let (router, _dir) = crate::api::tests::full_router().await;
        let mut misses = Vec::new();
        for (path, method, _) in paths(&document) {
            // A placeholder that every path parameter accepts as text; the
            // handler behind it may then answer 400 or 404 for the resource,
            // which is not what is being asked here. What tells a mounted
            // route from an unmounted one: the router answers an unmounted
            // path with the route fallback's literal two-key body, and a
            // mounted path with an unsupported method with 405.
            let uri = path.replace(['{', '}'], "");
            let (status, body) = send(
                &router,
                method.to_ascii_uppercase().as_str(),
                &uri,
                axum::body::Body::empty(),
                "application/json",
            )
            .await;
            let fell_through = status == StatusCode::NOT_FOUND
                && body["error"].is_string()
                && body["details"].is_string();
            if fell_through || status == StatusCode::METHOD_NOT_ALLOWED {
                misses.push(format!(
                    "{} {} ({status})",
                    method.to_ascii_uppercase(),
                    path
                ));
            }
        }
        assert!(
            misses.is_empty(),
            "operations described but not mounted under their prefix: {misses:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Examples: real answers, captured for the CLI to deserialise.
    // -----------------------------------------------------------------------

    /// One captured answer.
    #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct Example {
        #[serde(rename = "operationId")]
        operation_id: String,
        status: u16,
        body: Value,
    }

    const PLACEHOLDER_UUID: &str = "018f2518-7e38-7f80-8000-000000000001";
    const PLACEHOLDER_INSTANT: &str = "2026-01-01T00:00:00Z";
    const PLACEHOLDER_SECONDS: i64 = 1_767_225_600;

    /// Replace what differs between two runs - identifiers, instants,
    /// digests, cursors - with fixed values of the same shape, so the file
    /// is stable and still deserialises into the same types.
    fn normalise(key: Option<&str>, value: Value) -> Value {
        fn is_uuid(text: &str) -> bool {
            text.len() == 36 && uuid::Uuid::parse_str(text).is_ok()
        }
        fn is_instant(text: &str) -> bool {
            chrono::DateTime::parse_from_rfc3339(text).is_ok()
        }
        fn is_hex_digest(text: &str) -> bool {
            text.len() >= 40 && text.chars().all(|c| c.is_ascii_hexdigit())
        }
        fn is_cursor(key: Option<&str>) -> bool {
            matches!(key, Some("nextCursor" | "cursor"))
        }
        fn is_instant_key(key: Option<&str>) -> bool {
            key.is_some_and(|key| {
                key.ends_with("_at")
                    || key.ends_with("At")
                    || key.ends_with("_at_unix_seconds")
                    || key.ends_with("AtUnixSeconds")
                    || key == "timestamp"
                    || key == "at"
                    || key.ends_with("_timestamp")
            })
        }
        /// Identifiers that sit inside a longer string - a token id in an
        /// audit path, a version id in a bundle URL: every maximal run of
        /// hexadecimal digits and hyphens that is a UUID becomes the
        /// placeholder, and every run of at least sixteen hexadecimal
        /// digits becomes zeros of the same length.
        fn scrub(text: &str) -> String {
            let mut out = String::with_capacity(text.len());
            let mut run = String::new();
            let flush = |run: &mut String, out: &mut String| {
                if run.is_empty() {
                    return;
                }
                if is_uuid(run) {
                    out.push_str(PLACEHOLDER_UUID);
                } else if run.len() >= 16 && run.chars().all(|c| c.is_ascii_hexdigit()) {
                    out.push_str(&"0".repeat(run.len()));
                } else {
                    out.push_str(run);
                }
                run.clear();
            };
            for c in text.chars() {
                if c.is_ascii_hexdigit() || c == '-' {
                    run.push(c);
                } else {
                    flush(&mut run, &mut out);
                    out.push(c);
                }
            }
            flush(&mut run, &mut out);
            out
        }
        match value {
            Value::String(text) if is_uuid(&text) => Value::String(PLACEHOLDER_UUID.into()),
            Value::String(text) if is_instant(&text) => Value::String(PLACEHOLDER_INSTANT.into()),
            Value::String(text) if is_hex_digest(&text) => Value::String("0".repeat(text.len())),
            Value::String(_) if is_cursor(key) => Value::String("opaque-cursor".into()),
            Value::String(text) => Value::String(scrub(&text)),
            Value::Number(number) if is_instant_key(key) && number.is_i64() => {
                Value::Number(PLACEHOLDER_SECONDS.into())
            }
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| normalise(key, item)).collect())
            }
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (inner_key, inner) in map {
                    let normalised = normalise(Some(&inner_key), inner);
                    out.insert(inner_key, normalised);
                }
                Value::Object(out)
            }
            other => other,
        }
    }

    /// Call every route a test can drive without a live node or a provider
    /// driver and keep one answer per response schema.
    async fn capture_examples() -> std::collections::BTreeMap<String, Example> {
        use axum::body::Body;
        use serde_json::json;

        let mut examples = std::collections::BTreeMap::new();
        let mut keep = |schema: &str,
                        operation_id: &str,
                        expected: u16,
                        answer: (axum::http::StatusCode, Value)| {
            let (status, body) = answer;
            assert_eq!(
                status.as_u16(),
                expected,
                "{operation_id} answered {status} while capturing {schema}: {body}"
            );
            examples.entry(schema.to_string()).or_insert(Example {
                operation_id: operation_id.to_string(),
                status: expected,
                body: normalise(None, body),
            });
        };

        // The catalog, driven through its own harness: one node key, a
        // secret store, and a runtime stand-in that accepts deliveries.
        // Its routes are spelled in the legacy prefix; the bodies are the
        // same on /api/v1.
        let h = crate::api::catalog::tests::harness(true).await;
        let r = &h.router;
        keep(
            "App",
            "createApp",
            201,
            send_json(r, "POST", "/api/apps", json!({"id": "demo"})).await,
        );
        keep(
            "ControlListPage_App",
            "listApps",
            200,
            send_json(r, "GET", "/api/apps", Value::Null).await,
        );
        keep(
            "Environment",
            "createEnvironment",
            201,
            send_json(
                r,
                "POST",
                "/api/apps/demo/environments",
                json!({"name": "production"}),
            )
            .await,
        );
        keep(
            "ControlListPage_Environment",
            "listEnvironments",
            200,
            send_json(r, "GET", "/api/apps/demo/environments", Value::Null).await,
        );
        let uploaded = send(
            r,
            "POST",
            "/api/apps/demo/versions",
            Body::from(crate::catalog::bundle::tests::sample_bundle("demo")),
            "application/zip",
        )
        .await;
        assert_eq!(uploaded.0, 201, "uploadVersion: {}", uploaded.1);
        let version_id = uploaded.1["version"]["id"].as_str().unwrap().to_string();
        keep("UploadResponse", "uploadVersion", 201, uploaded);
        keep(
            "ControlListPage_VersionView",
            "listVersions",
            200,
            send_json(r, "GET", "/api/apps/demo/versions", Value::Null).await,
        );
        keep(
            "VersionView",
            "getVersion",
            200,
            send_json(
                r,
                "GET",
                &format!("/api/apps/demo/versions/{version_id}"),
                Value::Null,
            )
            .await,
        );
        let deployed = send_json(
            r,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            json!({"version_id": version_id}),
        )
        .await;
        let plan_id = deployed.1["plan"]["id"].as_str().map(str::to_string);
        keep(
            "DeploymentResponse",
            "deploy",
            deployed.0.as_u16(),
            deployed,
        );
        if let Some(plan_id) = plan_id {
            keep(
                "DeploymentPlanResponse",
                "getDeploymentPlan",
                200,
                send_json(
                    r,
                    "GET",
                    &format!("/api/deployment-plans/{plan_id}"),
                    Value::Null,
                )
                .await,
            );
        }
        keep(
            "ControlListPage_Deployment",
            "listDeployments",
            200,
            send_json(
                r,
                "GET",
                "/api/apps/demo/environments/production/deployments",
                Value::Null,
            )
            .await,
        );
        keep(
            "GrantResponse",
            "createDomainGrant",
            201,
            send_json(
                r,
                "POST",
                "/api/domains",
                json!({"pattern": "demo.example.com", "app_id": "demo"}),
            )
            .await,
        );
        keep(
            "ControlListPage_DomainGrant",
            "listDomainGrants",
            200,
            send_json(r, "GET", "/api/domains", Value::Null).await,
        );
        // A hostname is global, so the environment must hold a grant for
        // it before it may route it.
        let route = send_json(
            r,
            "POST",
            "/api/apps/demo/environments/production/routes",
            json!({"host": "demo.example.com", "path_prefix": "/", "priority": 0}),
        )
        .await;
        assert_eq!(route.0, 201, "createRoute: {}", route.1);
        let route_id = route.1["route"]["id"].as_str().unwrap().to_string();
        keep("RouteResponse", "createRoute", 201, route);
        keep(
            "ControlListPage_Route",
            "listRoutes",
            200,
            send_json(
                r,
                "GET",
                "/api/apps/demo/environments/production/routes",
                Value::Null,
            )
            .await,
        );
        keep(
            "GlobalRoutesResponse",
            "listAllRoutes",
            200,
            send_json(r, "GET", "/api/routes", Value::Null).await,
        );
        keep(
            "RouteDeleted",
            "deleteRoute",
            200,
            send_json(
                r,
                "DELETE",
                &format!("/api/apps/demo/environments/production/routes/{route_id}"),
                Value::Null,
            )
            .await,
        );
        keep(
            "SecretChangeResponse",
            "putSecret",
            200,
            send_json(
                r,
                "PUT",
                "/api/apps/demo/environments/production/secrets/API_KEY",
                json!({"value": "example-value"}),
            )
            .await,
        );
        keep(
            "ControlListPage_SecretMeta",
            "listSecrets",
            200,
            send_json(
                r,
                "GET",
                "/api/apps/demo/environments/production/secrets",
                Value::Null,
            )
            .await,
        );
        keep(
            "SecretDeletedResponse",
            "deleteSecret",
            200,
            send_json(
                r,
                "DELETE",
                "/api/apps/demo/environments/production/secrets/API_KEY",
                Value::Null,
            )
            .await,
        );
        keep(
            "GrantWithdrawn",
            "deleteDomainGrant",
            200,
            send_json(
                r,
                "DELETE",
                "/api/domains/demo.example.com?app_id=demo",
                Value::Null,
            )
            .await,
        );
        for (schema, operation_id, uri) in [
            (
                "ControlListPage_ProviderCredentialBinding",
                "listProviderCredentials",
                "/api/providers/credentials",
            ),
            (
                "ControlListPage_ProviderInstallation",
                "listProviderInstallations",
                "/api/providers/installations",
            ),
            (
                "ControlListPage_ProviderTarget",
                "listProviderTargets",
                "/api/providers/targets",
            ),
            (
                "ControlListPage_ProviderTargetGrant",
                "listProviderTargetGrantHistory",
                "/api/providers/target-grants",
            ),
            (
                "ControlListPage_RetiredAppName",
                "listRetiredApps",
                "/api/retired-apps",
            ),
        ] {
            keep(
                schema,
                operation_id,
                200,
                send_json(r, "GET", uri, Value::Null).await,
            );
        }
        keep(
            "DeleteAppResponse",
            "deleteApp",
            200,
            send_json(r, "DELETE", "/api/apps/demo", Value::Null).await,
        );
        keep(
            "ReleaseRetiredAppResponse",
            "releaseRetiredApp",
            200,
            send_json(r, "DELETE", "/api/retired-apps/demo", Value::Null).await,
        );

        // Everything else through the whole router, under the prefixes the
        // document describes.
        let (router, _dir) = crate::api::tests::full_router().await;
        let r = &router;
        keep(
            "ErrorBody",
            "getApp",
            404,
            send_json(r, "GET", "/api/v1/apps/no-such-app", Value::Null).await,
        );
        keep(
            "Account",
            "createAccount",
            200,
            send_json(
                r,
                "POST",
                "/api/v1/accounts",
                json!({"name": "alice", "role": "viewer"}),
            )
            .await,
        );
        keep(
            "ControlListPage_Account",
            "listAccounts",
            200,
            send_json(r, "GET", "/api/v1/accounts", Value::Null).await,
        );
        let minted = send_json(
            r,
            "POST",
            "/api/v1/accounts/alice/tokens",
            json!({"name": "laptop"}),
        )
        .await;
        assert_eq!(minted.0, 200, "{}", minted.1);
        let token_id = minted.1["token"]["id"].as_str().unwrap().to_string();
        keep(
            "ControlListPage_ApiToken",
            "listTokens",
            200,
            send_json(r, "GET", "/api/v1/tokens", Value::Null).await,
        );
        keep(
            "ApiToken",
            "revokeToken",
            200,
            send_json(
                r,
                "DELETE",
                &format!("/api/v1/tokens/{token_id}"),
                Value::Null,
            )
            .await,
        );
        keep(
            "ControlListPage_AuditEntry",
            "listAuditLog",
            200,
            send_json(r, "GET", "/api/v1/audit", Value::Null).await,
        );
        keep(
            "ProductInstallation",
            "getProductInstallation",
            200,
            send_json(r, "GET", "/api/v1/installation", Value::Null).await,
        );
        keep(
            "ConfigsUpdated",
            "updateNodeConfigs",
            200,
            send_json(
                r,
                "POST",
                "/api/platform/configs/node-a",
                json!({"configs": {"port": 8080}}),
            )
            .await,
        );
        keep(
            "ConfigsListing",
            "listNodeConfigs",
            200,
            send_json(r, "GET", "/api/platform/configs", Value::Null).await,
        );
        keep(
            "NodeConfigs",
            "getNodeConfigs",
            200,
            send_json(r, "GET", "/api/platform/configs/node-a", Value::Null).await,
        );
        keep(
            "ConfigValue",
            "getNodeConfigValue",
            200,
            send_json(r, "GET", "/api/platform/configs/node-a/port", Value::Null).await,
        );
        keep(
            "ControlListPage_ConfigHistoryItem",
            "listNodeConfigHistory",
            200,
            send_json(
                r,
                "GET",
                "/api/platform/configs/node-a/history",
                Value::Null,
            )
            .await,
        );
        keep(
            "ConfigStatsResponse",
            "getNodeConfigStats",
            200,
            send_json(r, "GET", "/api/platform/configs/stats", Value::Null).await,
        );
        keep(
            "ConfigKeyOutcome",
            "deleteNodeConfigValue",
            200,
            send_json(
                r,
                "DELETE",
                "/api/platform/configs/node-a/port",
                Value::Null,
            )
            .await,
        );
        keep(
            "NodeConfigOutcome",
            "deleteNodeConfigs",
            200,
            send_json(r, "DELETE", "/api/platform/configs/node-a", Value::Null).await,
        );
        for (schema, operation_id, uri) in [
            ("SessionsResponse", "listSessions", "/api/platform/sessions"),
            (
                "ControlListPage_CommandSummary",
                "listNodeCommands",
                "/api/platform/commands",
            ),
            (
                "ControlListPage_EventItem",
                "listEvents",
                "/api/platform/events",
            ),
            (
                "EventStatsResponse",
                "getEventStats",
                "/api/platform/events/stats",
            ),
            (
                "MetricsSummaryResponse",
                "getMetricsSummary",
                "/api/platform/metrics/summary",
            ),
            ("FleetResponse", "listRuntimeFleet", "/api/platform/runtime"),
        ] {
            keep(
                schema,
                operation_id,
                200,
                send_json(r, "GET", uri, Value::Null).await,
            );
        }
        examples
    }

    fn render_examples(examples: &std::collections::BTreeMap<String, Example>) -> String {
        let document = serde_json::json!({
            "$comment": "Captured from real handler answers by deis-control's openapi tests; identifiers, instants, digests and cursors are replaced by fixed placeholders. Regenerate with UPDATE_OPENAPI=1.",
            "openapiVersion": DOCUMENT_VERSION,
            "examples": examples,
        });
        let mut text = serde_json::to_string_pretty(&sorted(document)).unwrap();
        text.push('\n');
        text
    }

    /// `contracts/openapi/v1-examples.json` is one real answer per response
    /// schema the tests can produce, for the CLI to deserialise against.
    /// Regenerated with `UPDATE_OPENAPI=1`, and otherwise held equal.
    #[tokio::test]
    async fn the_committed_examples_are_what_the_handlers_answer() {
        let examples = capture_examples().await;
        let document = rendered();
        let schemas = document["components"]["schemas"].as_object().unwrap();
        for (schema, example) in &examples {
            assert!(
                schemas.contains_key(schema),
                "example {schema} names a schema the document does not define"
            );
            assert!(
                !looks_like_a_credential_value(&example.body),
                "example {schema} carries a credential-shaped value: {}",
                example.body
            );
        }
        let rendered_examples = render_examples(&examples);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(EXAMPLES_PATH);
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        if committed == rendered_examples {
            return;
        }
        if std::env::var_os("UPDATE_OPENAPI").is_some_and(|value| value == "1") {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &rendered_examples).unwrap();
            panic!("{EXAMPLES_PATH} regenerated from the handlers, commit it");
        }
        let committed: Value = serde_json::from_str(&committed).unwrap_or(Value::Null);
        let committed_names: BTreeSet<&str> = committed["examples"]
            .as_object()
            .map(|map| map.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let captured_names: BTreeSet<&str> = examples.keys().map(String::as_str).collect();
        let differing: Vec<&str> = examples
            .iter()
            .filter(|(name, example)| {
                serde_json::to_value(example).unwrap() != committed["examples"][name.as_str()]
            })
            .map(|(name, _)| name.as_str())
            .collect();
        panic!(
            "{EXAMPLES_PATH} does not match what the handlers answer. Missing: {:?}. Stale: {:?}. \
             Differing: {differing:?}. Run `UPDATE_OPENAPI=1 cargo test -p deis-control --lib \
             openapi::tests::the_committed_examples` to regenerate it, then commit the file.",
            captured_names
                .difference(&committed_names)
                .collect::<Vec<_>>(),
            committed_names
                .difference(&captured_names)
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn rendering_sorts_every_object_key() {
        let text = render();
        let value: Value = serde_json::from_str(&text).unwrap();
        fn check(value: &Value) {
            if let Some(map) = value.as_object() {
                let keys: Vec<&String> = map.keys().collect();
                let mut sorted_keys = keys.clone();
                sorted_keys.sort();
                assert_eq!(keys, sorted_keys);
                map.values().for_each(check);
            } else if let Some(items) = value.as_array() {
                items.iter().for_each(check);
            }
        }
        check(&value);
        assert!(text.ends_with('\n'));
    }
}
