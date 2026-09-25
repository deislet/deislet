//! The one error type handlers return, and the one body it renders to.
//!
//! An [`ApiError`] is a status class plus a message, as it always was; the
//! ~200 places that construct one are untouched. What is new is that every
//! variant also has a stable machine-readable [`codes`] entry, and that a
//! handler may enrich an error with a target, field violations, provider
//! detail or a retry hint through the builder methods.
//!
//! The body is the structured envelope `{"error": {code, message, requestId,
//! ...}}` on every answer. [`IntoResponse`] writes it with a freshly minted
//! request id and leaves the full [`ErrorDetail`] in the response
//! extensions; the renderers then re-render the body with the id the request
//! context settled - [`crate::api::versioning::render_errors`] mounted per
//! surface, or [`ApiError::into_response_for`] for the producers that build
//! their response outside every mount (the authorization gate, the route
//! fallback) - so the id inside the body is the one the response echoes.
//!
//! A failure on the control plane's own side - its database, a node it
//! dialled, a backend it drove - is described to the caller by
//! [`OPAQUE_MESSAGE`] and the request id only. The backend's own
//! text (an SQLite error, an io path, a node's gRPC address) is kept in
//! [`ErrorDetail::diagnostic`] for the server log, where the id finds it,
//! and never enters the envelope.

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use deis_provider_driver::{DriverErrorCategory, SideEffect};
use deis_wire::CanonicalId;
use serde::Serialize;

use crate::api::versioning::{ApiSurface, RequestContext, finish_error_response};

/// What a structured surface says about a failure whose real text belongs
/// in the log: fixed, so no backend detail can travel with it.
pub const OPAQUE_MESSAGE: &str =
    "The control plane could not complete the request; quote the request id to an operator.";

/// The header a keyed mutation replays under, as HTTP compares header
/// names. [`ApiError::missing_header`] recognises it to give the one
/// omission a client acts on differently its own code.
const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";

/// Stable, machine-readable error codes.
///
/// Kebab-case, never renamed once shipped: a client branches on these where
/// it used to grep messages. The provider-lifecycle and idempotency entries
/// are the very tokens the catalog already prefixes onto its messages, so a
/// code here and the prefix there cannot drift apart.
pub mod codes {
    pub const NOT_FOUND: &str = "not-found";
    pub const INVALID_ARGUMENT: &str = "invalid-argument";
    /// A `limit` query parameter outside `1..=1000`; see
    /// [`crate::api::pagination::bounded_limit`], which is the one place
    /// every list route bounds it.
    pub const LIMIT_OUT_OF_RANGE: &str = "limit-out-of-range";
    /// Reserved, and emitted by nothing: offset paging was retired before
    /// any route shipped it, and every list is keyset-paged behind an
    /// opaque cursor. The token stays in the closed enum - a code is never
    /// removed once published, and a client that already branches on it
    /// keeps compiling - and this is the one place that says so.
    pub const OFFSET_OUT_OF_RANGE: &str = "offset-out-of-range";
    /// A cursor that was not issued for this collection and these filters,
    /// or has expired; see [`crate::catalog::INVALID_CURSOR_MESSAGE`].
    pub const CURSOR_INVALID: &str = "cursor-invalid";
    /// A mutation that replays under a key was sent without one.
    pub const IDEMPOTENCY_KEY_REQUIRED: &str = "idempotency-key-required";
    pub const IDEMPOTENCY_KEY_REUSED: &str = crate::catalog::idempotency::IDEMPOTENCY_KEY_REUSED;
    pub const IDEMPOTENCY_REQUEST_IN_PROGRESS: &str =
        crate::catalog::idempotency::IDEMPOTENCY_REQUEST_IN_PROGRESS;
    pub const PRECONDITION_REQUIRED: &str = "precondition-required";
    pub const REVISION_CONFLICT: &str = "revision-conflict";
    /// A create refused because the resource it names is already there;
    /// see [`super::ApiError::creating`].
    pub const ALREADY_EXISTS: &str = "already-exists";
    /// A 409 that is none of the more specific conflicts: the request is
    /// well formed and clashes with the current state.
    pub const CONFLICT: &str = "conflict";
    pub const NOT_CONFIGURED: &str = "not-configured";
    pub const FORBIDDEN: &str = "forbidden";
    pub const INSUFFICIENT_SCOPE: &str = "insufficient-scope";
    pub const UNAUTHENTICATED: &str = "unauthenticated";
    pub const PAYLOAD_TOO_LARGE: &str = "payload-too-large";
    pub const SERVICE_UNAVAILABLE: &str = "service-unavailable";
    pub const BACKEND_UNAVAILABLE: &str = "backend-unavailable";
    pub const DATABASE_UNAVAILABLE: &str = "database-unavailable";
    pub const INTERNAL: &str = "internal";
    pub const ROUTE_NOT_FOUND: &str = "route-not-found";
    pub const PROVIDER_INSTALLATION_HAS_DEPENDENTS: &str =
        crate::catalog::PROVIDER_INSTALLATION_HAS_DEPENDENTS;
    pub const PROVIDER_INSTALLATION_IN_USE: &str = crate::catalog::PROVIDER_INSTALLATION_IN_USE;
    pub const PROVIDER_TARGET_IN_USE: &str = crate::catalog::PROVIDER_TARGET_IN_USE;
    pub const PROVIDER_RESOURCE_DELETED: &str = crate::catalog::PROVIDER_RESOURCE_DELETED;
    /// A cancel of an operation that is terminal or already cancelling.
    pub const OPERATION_NOT_CANCELLABLE: &str = crate::catalog::OPERATION_NOT_CANCELLABLE;
    /// A rollback whose source is not a succeeded deployment on the named
    /// environment, or whose authorization has since changed.
    pub const OPERATION_NOT_ROLLBACKABLE: &str = crate::catalog::OPERATION_NOT_ROLLBACKABLE;

    /// Every code, for the contract tests and the OpenAPI enum.
    pub const ALL: &[&str] = &[
        NOT_FOUND,
        INVALID_ARGUMENT,
        LIMIT_OUT_OF_RANGE,
        OFFSET_OUT_OF_RANGE,
        CURSOR_INVALID,
        IDEMPOTENCY_KEY_REQUIRED,
        IDEMPOTENCY_KEY_REUSED,
        IDEMPOTENCY_REQUEST_IN_PROGRESS,
        PRECONDITION_REQUIRED,
        REVISION_CONFLICT,
        ALREADY_EXISTS,
        CONFLICT,
        NOT_CONFIGURED,
        FORBIDDEN,
        INSUFFICIENT_SCOPE,
        UNAUTHENTICATED,
        PAYLOAD_TOO_LARGE,
        SERVICE_UNAVAILABLE,
        BACKEND_UNAVAILABLE,
        DATABASE_UNAVAILABLE,
        INTERNAL,
        ROUTE_NOT_FOUND,
        PROVIDER_INSTALLATION_HAS_DEPENDENTS,
        PROVIDER_INSTALLATION_IN_USE,
        PROVIDER_TARGET_IN_USE,
        PROVIDER_RESOURCE_DELETED,
        OPERATION_NOT_CANCELLABLE,
        OPERATION_NOT_ROLLBACKABLE,
    ];

    /// Codes the catalog already spells out as a `<code>: ` prefix on the
    /// message. The structured body lifts the token into `code` and drops
    /// it from `message`; the legacy string keeps it.
    pub const MESSAGE_PREFIXES: &[&str] = &[
        IDEMPOTENCY_KEY_REUSED,
        IDEMPOTENCY_REQUEST_IN_PROGRESS,
        PROVIDER_INSTALLATION_HAS_DEPENDENTS,
        PROVIDER_INSTALLATION_IN_USE,
        PROVIDER_TARGET_IN_USE,
        PROVIDER_RESOURCE_DELETED,
        OPERATION_NOT_CANCELLABLE,
        OPERATION_NOT_ROLLBACKABLE,
    ];

    /// Whether a code names a condition that a retry of the same request,
    /// unchanged, may clear.
    pub const fn is_retryable(code: &str) -> bool {
        // `const fn` cannot compare `&str` with `==`; byte comparison keeps
        // the table usable in constants.
        const fn eq(a: &str, b: &str) -> bool {
            let (a, b) = (a.as_bytes(), b.as_bytes());
            if a.len() != b.len() {
                return false;
            }
            let mut i = 0;
            while i < a.len() {
                if a[i] != b[i] {
                    return false;
                }
                i += 1;
            }
            true
        }
        eq(code, SERVICE_UNAVAILABLE)
            || eq(code, BACKEND_UNAVAILABLE)
            || eq(code, DATABASE_UNAVAILABLE)
            || eq(code, IDEMPOTENCY_REQUEST_IN_PROGRESS)
    }

    /// Split `"<code>: rest"` into `(code, rest)` when `<code>` is one of
    /// [`MESSAGE_PREFIXES`].
    pub fn split_prefix(message: &str) -> Option<(&'static str, &str)> {
        MESSAGE_PREFIXES.iter().find_map(|code| {
            message
                .strip_prefix(code)
                .and_then(|rest| rest.strip_prefix(": "))
                .map(|rest| (*code, rest))
        })
    }
}

/// What an error is about, when it is about one thing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorTarget {
    /// The resource kind or path the error concerns, e.g. `application`.
    pub resource: String,
    /// The field within it, for a validation failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// One field that failed validation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Violation {
    pub field: String,
    pub code: &'static str,
    pub message: String,
}

/// What a provider driver said about a failure, in the closed vocabulary of
/// `contracts/v1/provider-driver-process.schema.json`'s `driverError` and
/// nothing else: the envelope's `provider` member is this object, so a
/// driver's free text, its stderr or anything it was configured with cannot
/// be forwarded by accident.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDiagnostic {
    /// The backend the driver drives, e.g. `cloudflare-workers-pages`.
    pub backend_id: String,
    pub category: DriverErrorCategory,
    /// The driver's own failure class, `SCREAMING_SNAKE_CASE`.
    pub code: String,
    pub side_effect: SideEffect,
    /// The provider's own request id, when the driver was given one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Everything known about an error, in the form the envelope is rendered
/// from.
///
/// Left in the response extensions by [`IntoResponse`] for
/// [`crate::api::versioning::render_errors`] to pick up.
#[derive(Clone, Debug)]
pub struct ErrorDetail {
    pub status: StatusCode,
    pub code: &'static str,
    /// The message the envelope carries: without a leading code token and
    /// without the `Database error: ` prefix.
    pub message: String,
    /// The backend's own text for a failure the envelope describes with
    /// [`OPAQUE_MESSAGE`]. For the server log only, where the request id
    /// finds it; never serialised into the envelope.
    pub diagnostic: Option<String>,
    /// What the audit record of this answer says, when the handler that
    /// produced it knows more than the status: a refusal whose exact demand
    /// was read from a record the body must not describe. The authorization
    /// gate copies it into the record's `detail` and nothing else reads it;
    /// never serialised into either body.
    pub audit_detail: Option<String>,
    pub target: Option<ErrorTarget>,
    pub violations: Vec<Violation>,
    /// A property of the code, as the contract says: set from
    /// [`codes::is_retryable`] whenever the code is.
    pub retryable: bool,
    pub retry_after_seconds: Option<u64>,
    pub docs: Option<String>,
    pub provider: Option<ProviderDiagnostic>,
}

impl ErrorDetail {
    fn new(status: StatusCode, code: &'static str, message: String) -> Self {
        Self {
            status,
            code,
            message,
            diagnostic: None,
            audit_detail: None,
            target: None,
            violations: Vec::new(),
            retryable: codes::is_retryable(code),
            retry_after_seconds: None,
            docs: None,
            provider: None,
        }
    }

    /// A failure on the control plane's own side: the message is
    /// [`OPAQUE_MESSAGE`] and `diagnostic` is what the log gets.
    fn opaque(status: StatusCode, code: &'static str, diagnostic: String) -> Self {
        let mut detail = Self::new(status, code, OPAQUE_MESSAGE.to_string());
        detail.diagnostic = Some(diagnostic);
        detail
    }

    /// Serialise the structured envelope with `request_id` filled in.
    pub fn structured_body(&self, request_id: &str) -> Vec<u8> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            error: ErrorBody<'a>,
        }
        let envelope = Envelope {
            error: ErrorBody {
                code: self.code,
                message: &self.message,
                request_id,
                target: self.target.as_ref(),
                violations: &self.violations,
                retryable: self.retryable,
                retry_after_seconds: self.retry_after_seconds,
                docs: self.docs.as_deref(),
                provider: self.provider.as_ref(),
            },
        };
        serde_json::to_vec(&envelope).expect("an error envelope holds only serialisable values")
    }
}

/// The structured envelope's `error` object, as a client sees it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody<'a> {
    code: &'static str,
    message: &'a str,
    request_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'a ErrorTarget>,
    violations: &'a [Violation],
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docs: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<&'a ProviderDiagnostic>,
}

/// API error type
#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    /// A mutation requires a resource revision but the caller omitted it.
    PreconditionRequired(String),
    /// The supplied resource revision is stale.
    PreconditionFailed(String),
    /// The request is well formed but clashes with existing state, for
    /// instance creating an app that already exists.
    Conflict(String),
    /// A dependency this endpoint needs is not configured or not usable,
    /// for instance the secret master key. Retrying without fixing the
    /// control plane will not help, but the request itself was fine.
    ServiceUnavailable(String),
    /// This control plane is deliberately not set up to do what was asked -
    /// a storage token with no `storage_auth_secret`, say.
    ///
    /// Deliberately 4xx and not [`Self::ServiceUnavailable`]: nothing here is
    /// unavailable or failing, and a 5xx tells every client and every proxy
    /// in between to retry something that will answer the same way forever.
    /// The message names the configuration key, because the caller is the
    /// one who has to go and ask for it to be set.
    NotConfigured(String),
    /// The caller authenticated and is not entitled to what the request
    /// contains.
    ///
    /// Its own variant rather than a [`Self::BadRequest`] because the request
    /// is perfectly well formed and would succeed for a wider credential, and
    /// because `crate::api::authz` records every 403 in the audit log whatever
    /// route produced it - so a refusal here is written down beside the ones
    /// the route gate makes.
    Forbidden(String),
    InternalError(String),
    DatabaseError(String),
    /// An error that already carries its full detail: a specific code, a
    /// status the plain variants cannot express, a target, violations, a
    /// retry hint. Made by the builder methods and the named constructors,
    /// never by hand at a call site.
    Detailed(Box<ErrorDetail>),
}

impl ApiError {
    /// An error with an explicit status and code.
    pub fn coded(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        let message = message.into();
        ApiError::Detailed(Box::new(ErrorDetail::new(status, code, message)))
    }

    /// A failure on the control plane's own side whose text is a backend's
    /// (a node's transport error, a store's refusal). The envelope says
    /// [`OPAQUE_MESSAGE`] and the log gets the text under the request id.
    pub fn opaque(status: StatusCode, code: &'static str, diagnostic: impl Into<String>) -> Self {
        ApiError::Detailed(Box::new(ErrorDetail::opaque(
            status,
            code,
            diagnostic.into(),
        )))
    }

    /// The authorization gate's "no acceptable credential" answer.
    pub fn unauthenticated(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        ApiError::Detailed(Box::new(ErrorDetail::new(
            StatusCode::UNAUTHORIZED,
            codes::UNAUTHENTICATED,
            detail,
        )))
    }

    /// The authorization gate's "authenticated but not for this" answer.
    /// The sentence says what the call needed, and the hint says how to get
    /// it, so a caller with the right answer can act without asking.
    pub fn insufficient_scope(required_scope: &str, detail: &str) -> Self {
        let hint = format!(
            "A token carrying {required_scope} would be allowed. Ask an owner for one with \
             `deis token create --account <you> --scope {required_scope}`."
        );
        ApiError::Detailed(Box::new(ErrorDetail::new(
            StatusCode::FORBIDDEN,
            codes::INSUFFICIENT_SCOPE,
            format!("{detail}. {hint}"),
        )))
    }

    /// The answer for an `/api/` path that matched no route.
    pub fn route_not_found() -> Self {
        ApiError::Detailed(Box::new(ErrorDetail::new(
            StatusCode::NOT_FOUND,
            codes::ROUTE_NOT_FOUND,
            "No such endpoint. The API surface is listed in the component's \
             API_DOCUMENTATION.md."
                .to_string(),
        )))
    }

    /// A required request header that was not sent.
    ///
    /// `Idempotency-Key` takes its own code because a client that omitted it
    /// has to change the request in a particular way - mint a key and send
    /// it - rather than fix a value it already chose, and that is worth
    /// branching on.
    pub fn missing_header(name: &str) -> Self {
        let message = format!("{name} header is required");
        if name.eq_ignore_ascii_case(IDEMPOTENCY_KEY_HEADER) {
            return Self::coded(
                StatusCode::BAD_REQUEST,
                codes::IDEMPOTENCY_KEY_REQUIRED,
                message,
            )
            // Named as a caller writes it, not as HTTP compares it.
            .with_target("request", Some("Idempotency-Key".to_string()));
        }
        ApiError::BadRequest(message)
    }

    /// A body larger than the route will read.
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::coded(
            StatusCode::PAYLOAD_TOO_LARGE,
            codes::PAYLOAD_TOO_LARGE,
            message,
        )
    }

    /// Replace the code while keeping status and messages, for a route
    /// that knows a more specific one than the variant implies -
    /// `ApiError::BadRequest(msg).with_code(codes::LIMIT_OUT_OF_RANGE)`.
    ///
    /// `retryable` follows the new code, and a retry hint that no longer
    /// applies goes with the old one.
    pub fn with_code(self, code: &'static str) -> Self {
        let mut detail = self.into_detail();
        detail.code = code;
        detail.retryable = codes::is_retryable(code);
        if !detail.retryable {
            detail.retry_after_seconds = None;
        }
        ApiError::Detailed(Box::new(detail))
    }

    /// What a create makes of a refusal.
    ///
    /// A plain 409 answering a create says one thing: the resource is
    /// already there. It answers [`codes::ALREADY_EXISTS`] about
    /// `resource`, so a client can tell "pick another name" from "the state
    /// moved under you" without reading the sentence. Everything else goes
    /// through untouched - a 404 for the parent, a validation refusal, and
    /// any 409 that already carries a code of its own (`not-configured`, a
    /// reused idempotency key, a provider-lifecycle conflict).
    ///
    /// The message is the store's, unchanged.
    pub fn creating(self, resource: &str) -> Self {
        let (status, code) = self.classify();
        if status != StatusCode::CONFLICT || code != codes::CONFLICT {
            return self;
        }
        self.with_code(codes::ALREADY_EXISTS)
            .with_target(resource, None)
    }

    /// Name the one request member a validation refusal is about.
    ///
    /// Only for a 400 the route can attribute to a single field with no
    /// guessing; the message stays what it was and becomes the violation's,
    /// so the envelope gains `violations: [{field, code: "invalid", message}]`.
    pub fn invalid_field(self, field: &str) -> Self {
        let (status, code) = self.classify();
        if status != StatusCode::BAD_REQUEST || code != codes::INVALID_ARGUMENT {
            return self;
        }
        let mut detail = self.into_detail();
        detail.violations = vec![Violation {
            field: field.to_string(),
            code: "invalid",
            message: detail.message.clone(),
        }];
        ApiError::Detailed(Box::new(detail))
    }

    pub fn with_target(self, resource: impl Into<String>, field: Option<String>) -> Self {
        let mut detail = self.into_detail();
        detail.target = Some(ErrorTarget {
            resource: resource.into(),
            field,
        });
        ApiError::Detailed(Box::new(detail))
    }

    pub fn with_violations(self, violations: Vec<Violation>) -> Self {
        let mut detail = self.into_detail();
        detail.violations = violations;
        ApiError::Detailed(Box::new(detail))
    }

    /// Attach what a provider driver said, in its closed vocabulary, for
    /// the caller to show.
    pub fn with_provider(self, provider: ProviderDiagnostic) -> Self {
        let mut detail = self.into_detail();
        detail.provider = Some(provider);
        ApiError::Detailed(Box::new(detail))
    }

    /// Advise waiting `seconds` before the retry.
    ///
    /// `retryable` is a property of the code and this does not change it: a
    /// hint on a code that is not retryable is a programming error, refused
    /// in debug builds and dropped in release ones, so the envelope never
    /// advises retrying a request that will answer the same way.
    pub fn retry_after(self, seconds: u64) -> Self {
        let mut detail = self.into_detail();
        debug_assert!(
            detail.retryable,
            "a retry hint on the non-retryable code {}",
            detail.code
        );
        if detail.retryable {
            detail.retry_after_seconds = Some(seconds);
        }
        ApiError::Detailed(Box::new(detail))
    }

    pub fn with_docs(self, url: impl Into<String>) -> Self {
        let mut detail = self.into_detail();
        detail.docs = Some(url.into());
        ApiError::Detailed(Box::new(detail))
    }

    pub fn status(&self) -> StatusCode {
        self.classify().0
    }

    pub fn code(&self) -> &'static str {
        self.classify().1
    }

    /// Status and code of any variant.
    ///
    /// Exhaustive on purpose: a new variant must be given a code here
    /// before it compiles. A plain variant whose message starts with one of
    /// [`codes::MESSAGE_PREFIXES`] takes that token as its code.
    fn classify(&self) -> (StatusCode, &'static str) {
        let (status, code, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, codes::NOT_FOUND, msg),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, codes::INVALID_ARGUMENT, msg),
            ApiError::PreconditionRequired(msg) => (
                StatusCode::PRECONDITION_REQUIRED,
                codes::PRECONDITION_REQUIRED,
                msg,
            ),
            ApiError::PreconditionFailed(msg) => (
                StatusCode::PRECONDITION_FAILED,
                codes::REVISION_CONFLICT,
                msg,
            ),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, codes::CONFLICT, msg),
            ApiError::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::SERVICE_UNAVAILABLE,
                msg,
            ),
            ApiError::NotConfigured(msg) => (StatusCode::CONFLICT, codes::NOT_CONFIGURED, msg),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, codes::FORBIDDEN, msg),
            ApiError::InternalError(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, codes::INTERNAL, msg)
            }
            ApiError::DatabaseError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::DATABASE_UNAVAILABLE,
                msg,
            ),
            ApiError::Detailed(detail) => return (detail.status, detail.code),
        };
        match codes::split_prefix(message) {
            Some((token, _)) => (status, token),
            None => (status, code),
        }
    }

    /// The same answer, with `detail` written into its audit record. The
    /// bodies do not change: this is for a handler whose refusal must stay
    /// silent about *why* to the caller (see [`Self::insufficient_scope`]
    /// on the generic operation routes) and still say so in the audit log,
    /// exactly as the gate's own refusals do.
    pub fn audited_as(self, detail: impl Into<String>) -> Self {
        let mut error = self.into_detail();
        error.audit_detail = Some(detail.into());
        ApiError::Detailed(Box::new(error))
    }

    /// Resolve into the full detail the envelope is rendered from.
    pub fn into_detail(self) -> ErrorDetail {
        let (status, code) = self.classify();
        let message = match self {
            ApiError::Detailed(detail) => return *detail,
            ApiError::NotFound(msg)
            | ApiError::BadRequest(msg)
            | ApiError::PreconditionRequired(msg)
            | ApiError::PreconditionFailed(msg)
            | ApiError::Conflict(msg)
            | ApiError::ServiceUnavailable(msg)
            | ApiError::NotConfigured(msg)
            | ApiError::Forbidden(msg) => msg,
            // Both carry whatever text the failing component produced, at
            // ~200 call sites: the log gets it, the envelope does not.
            ApiError::InternalError(msg) | ApiError::DatabaseError(msg) => {
                return ErrorDetail::opaque(status, code, msg);
            }
        };
        // A leading code token is lifted into `code` and dropped from the
        // message it would otherwise travel with.
        let message = match codes::split_prefix(&message) {
            Some((_, rest)) => rest.to_string(),
            None => message,
        };
        ErrorDetail::new(status, code, message)
    }

    /// Render for a producer that runs outside any surface mount, echoing
    /// the request and correlation ids the caller is to quote.
    /// [`RequestContext::resolve`] settles both, whether or not the context
    /// middleware has run. `surface` only names the answer in the log: the
    /// body is the envelope either way.
    pub fn into_response_for(self, surface: ApiSurface, context: RequestContext) -> Response {
        finish_error_response(surface, context, self.into_response())
    }
}

impl IntoResponse for ApiError {
    /// The structured envelope, with a request id minted here, and the
    /// detail left in the extensions for the renderer - the surface's
    /// `render_errors`, or [`Self::into_response_for`] for a producer
    /// outside every mount - to re-render it with the id the request
    /// context settled, which is the one the response echoes.
    fn into_response(self) -> Response {
        let detail = self.into_detail();
        let status = detail.status;
        let request_id = CanonicalId::new_v7();
        let body = detail.structured_body(&request_id.to_string());
        let mut response = (
            status,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            body,
        )
            .into_response();
        response.extensions_mut().insert(detail);
        response
    }
}

impl From<crate::database::DatabaseError> for ApiError {
    fn from(err: crate::database::DatabaseError) -> Self {
        ApiError::DatabaseError(err.to_string())
    }
}

/// Map catalog failures onto HTTP status classes. The variants were
/// chosen so this stays a straight table with no guessing.
///
/// The one refusal that is finer than its variant is the cursor store's:
/// `PageCursorStore::resolve` reports a cursor it did not issue for this
/// query as a plain [`CatalogError::Invalid`], and it is the same sentence
/// every time - [`crate::catalog::INVALID_CURSOR_MESSAGE`], which that
/// module owns and this reads. Recognising it here rather than adding a
/// variant keeps the ~40 list routes free of a mapping each.
impl From<crate::catalog::CatalogError> for ApiError {
    fn from(err: crate::catalog::CatalogError) -> Self {
        use crate::catalog::CatalogError;
        let message = err.to_string();
        match err {
            CatalogError::Invalid(ref reason)
                if reason == crate::catalog::INVALID_CURSOR_MESSAGE =>
            {
                ApiError::coded(StatusCode::BAD_REQUEST, codes::CURSOR_INVALID, message)
                    .with_target("query", Some("cursor".to_string()))
            }
            CatalogError::NotFound(_) => ApiError::NotFound(message),
            CatalogError::Conflict(_) => ApiError::Conflict(message),
            CatalogError::RevisionConflict(_) => ApiError::PreconditionFailed(message),
            CatalogError::Invalid(_) => ApiError::BadRequest(message),
            CatalogError::Forbidden(_) => ApiError::Forbidden(message),
            CatalogError::BundleStorage(_) => ApiError::InternalError(message),
            // The secret store's own text names the master key file by its
            // absolute path, and a caller holding one environment's secret
            // scope must not learn where an operator keeps it. Opaque like
            // every other 5xx: the path goes to the log under the request
            // id.
            CatalogError::SecretsUnavailable(_) => ApiError::opaque(
                StatusCode::SERVICE_UNAVAILABLE,
                codes::SERVICE_UNAVAILABLE,
                message,
            ),
            CatalogError::Database(_) | CatalogError::Backend(_) => {
                ApiError::DatabaseError(message)
            }
        }
    }
}

impl From<crate::catalog::MigrationCoordinatorError> for ApiError {
    fn from(err: crate::catalog::MigrationCoordinatorError) -> Self {
        use crate::catalog::MigrationCoordinatorError;
        match err {
            MigrationCoordinatorError::Configuration(message) => ApiError::NotConfigured(message),
            MigrationCoordinatorError::Catalog(error) => error.into(),
            // The store or queue backend, not this process, is what failed;
            // what it said names the backend and the tenant, so the log
            // gets it and the envelope does not.
            MigrationCoordinatorError::Backend { .. }
            | MigrationCoordinatorError::Protocol { .. } => ApiError::opaque(
                StatusCode::SERVICE_UNAVAILABLE,
                codes::BACKEND_UNAVAILABLE,
                err.to_string(),
            ),
        }
    }
}

impl From<crate::catalog::BackendRestoreCoordinatorError> for ApiError {
    fn from(err: crate::catalog::BackendRestoreCoordinatorError) -> Self {
        use crate::catalog::BackendRestoreCoordinatorError;
        match err {
            BackendRestoreCoordinatorError::Configuration(message) => {
                ApiError::NotConfigured(message)
            }
            BackendRestoreCoordinatorError::Catalog(error) => error.into(),
            BackendRestoreCoordinatorError::Unknown(message)
            | BackendRestoreCoordinatorError::Protocol(message) => ApiError::opaque(
                StatusCode::SERVICE_UNAVAILABLE,
                codes::BACKEND_UNAVAILABLE,
                message,
            ),
        }
    }
}

/// Map configuration-push failures onto HTTP status classes.
///
/// The same reasoning as the command mapping below: a push travels down a
/// control channel the node is holding open, so a node that is not connected,
/// or a configuration key that was never stored, is the caller naming
/// something that is not there rather than a fault in this process. Both used
/// to come back as 500.
impl From<crate::config_manager::PushError> for ApiError {
    fn from(err: crate::config_manager::PushError) -> Self {
        use crate::config_manager::PushError;
        let message = err.to_string();
        match err {
            PushError::NodeNotFound(_) => ApiError::NotFound(format!(
                "{message} - no node by that name is holding a control channel open, so nothing \
                 can be pushed to it. A deis-runtime node never holds one: its configuration \
                 travels with its deployment"
            )),
            PushError::ConfigNotFound(_) => ApiError::NotFound(message),
            // The channel was there when the send began and went away
            // underneath; the caller can retry, this instance cannot fix it.
            // What the channel said is about this process's own plumbing, so
            // it goes to the log and not into the envelope.
            PushError::SendError(_) => ApiError::opaque(
                StatusCode::SERVICE_UNAVAILABLE,
                codes::SERVICE_UNAVAILABLE,
                message,
            ),
            PushError::SerializationError(_) => ApiError::InternalError(message),
        }
    }
}

/// Map command failures onto HTTP status classes.
///
/// The one that matters is `NodeNotFound`: a command can only travel down a
/// control channel the node is holding open, so asking to command a node
/// that is not connected is the caller naming something that is not there.
/// Reporting that as a 500 blamed the control plane for a fact about the
/// fleet, and the dashboard showed it as a server failure.
impl From<crate::command::ExecuteError> for ApiError {
    fn from(err: crate::command::ExecuteError) -> Self {
        use crate::command::ExecuteError;
        let message = err.to_string();
        match err {
            ExecuteError::NodeNotFound(_) => ApiError::NotFound(format!(
                "{message} - no node by that name is holding a control channel open, so nothing can be sent to it"
            )),
            ExecuteError::CommandNotFound => ApiError::NotFound(message),
            ExecuteError::InvalidTimeout => ApiError::BadRequest(message),
            ExecuteError::DatabaseError(_) => ApiError::DatabaseError(message),
            // The node was there when the send began and its channel went
            // away underneath: the caller can retry, this instance cannot
            // fix it.
            ExecuteError::SendError(_) | ExecuteError::ChannelClosed => ApiError::opaque(
                StatusCode::SERVICE_UNAVAILABLE,
                codes::SERVICE_UNAVAILABLE,
                message,
            ),
            ExecuteError::SerializationError(_) | ExecuteError::Timeout => {
                ApiError::InternalError(message)
            }
        }
    }
}

/// Map failures of the gRPC management plane to a runtime node.
///
/// The status is the node's, not this process's: an unreachable node is a
/// bad gateway, a node whose key was retired is a conflict it clears by
/// registering again. A transport failure spells out the node's management
/// address, so it is opaque and the log has it.
impl From<crate::runtime_client::NodeError> for ApiError {
    fn from(err: crate::runtime_client::NodeError) -> Self {
        use crate::runtime_client::NodeError;
        let message = err.to_string();
        match err {
            NodeError::UnknownNode => ApiError::NotFound(message),
            NodeError::Transport(_) | NodeError::Rpc { .. } => {
                ApiError::opaque(StatusCode::BAD_GATEWAY, codes::BACKEND_UNAVAILABLE, message)
            }
            // Nothing was dialled: the node is on the list but the key it
            // proved is no longer one this control plane holds. Not the
            // caller's fault and not a gateway failure - the node has to
            // register again.
            NodeError::NotReachable { .. } => ApiError::Conflict(message),
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    const REQUEST: &str = "018f2518-7e38-7f80-8000-000000000001";

    /// One instance of every variant, with the code and status the contract
    /// promises for it. A new variant fails `classify` first and this table
    /// second.
    fn every_variant() -> Vec<(ApiError, StatusCode, &'static str)> {
        vec![
            (
                ApiError::NotFound("x".into()),
                StatusCode::NOT_FOUND,
                codes::NOT_FOUND,
            ),
            (
                ApiError::BadRequest("x".into()),
                StatusCode::BAD_REQUEST,
                codes::INVALID_ARGUMENT,
            ),
            (
                ApiError::PreconditionRequired("x".into()),
                StatusCode::PRECONDITION_REQUIRED,
                codes::PRECONDITION_REQUIRED,
            ),
            (
                ApiError::PreconditionFailed("x".into()),
                StatusCode::PRECONDITION_FAILED,
                codes::REVISION_CONFLICT,
            ),
            (
                ApiError::Conflict("x".into()),
                StatusCode::CONFLICT,
                codes::CONFLICT,
            ),
            (
                ApiError::ServiceUnavailable("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                codes::SERVICE_UNAVAILABLE,
            ),
            (
                ApiError::NotConfigured("x".into()),
                StatusCode::CONFLICT,
                codes::NOT_CONFIGURED,
            ),
            (
                ApiError::Forbidden("x".into()),
                StatusCode::FORBIDDEN,
                codes::FORBIDDEN,
            ),
            (
                ApiError::InternalError("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::INTERNAL,
            ),
            (
                ApiError::DatabaseError("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::DATABASE_UNAVAILABLE,
            ),
            (
                ApiError::coded(StatusCode::BAD_GATEWAY, codes::BACKEND_UNAVAILABLE, "x"),
                StatusCode::BAD_GATEWAY,
                codes::BACKEND_UNAVAILABLE,
            ),
            (
                ApiError::opaque(StatusCode::BAD_GATEWAY, codes::BACKEND_UNAVAILABLE, "x"),
                StatusCode::BAD_GATEWAY,
                codes::BACKEND_UNAVAILABLE,
            ),
        ]
    }

    fn contracts_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/v1")
    }

    fn read_json(path: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!("{}: {error}", path.display());
        }))
        .unwrap()
    }

    fn provider_diagnostic() -> ProviderDiagnostic {
        ProviderDiagnostic {
            backend_id: "cloudflare-workers-pages".to_string(),
            category: DriverErrorCategory::Configuration,
            code: "ZONE_NOT_VISIBLE".to_string(),
            side_effect: SideEffect::None,
            request_id: Some("0cb2a4e1-4f2a-4c47-9e3b-4a2d0c5f7ab1".to_string()),
        }
    }

    async fn into_response_bytes(error: ApiError) -> (StatusCode, Vec<u8>) {
        let response = error.into_response();
        let status = response.status();
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    fn structured(error: ApiError) -> Value {
        let detail = error.into_detail();
        serde_json::from_slice(&detail.structured_body(REQUEST)).unwrap()
    }

    #[test]
    fn codes_are_unique_and_kebab_case() {
        let mut seen = BTreeSet::new();
        for code in codes::ALL {
            assert!(seen.insert(*code), "{code} is listed twice");
            assert!(
                !code.is_empty()
                    && code
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && !code.starts_with('-')
                    && !code.ends_with('-')
                    && !code.contains("--"),
                "{code} is not kebab-case"
            );
        }
        for prefix in codes::MESSAGE_PREFIXES {
            assert!(seen.contains(prefix), "{prefix} is a prefix but not a code");
        }
        // The four retryable codes, as `scripts/check-shared-contracts.py`
        // and the OpenAPI `ErrorCode` description list them.
        let retryable: BTreeSet<&str> = codes::ALL
            .iter()
            .copied()
            .filter(|code| codes::is_retryable(code))
            .collect();
        assert_eq!(
            retryable,
            BTreeSet::from([
                codes::SERVICE_UNAVAILABLE,
                codes::BACKEND_UNAVAILABLE,
                codes::DATABASE_UNAVAILABLE,
                codes::IDEMPOTENCY_REQUEST_IN_PROGRESS,
            ])
        );
    }

    /// Every failure a backend hands one of the `From` impls answers 5xx
    /// with the fixed message and keeps the backend's own sentence in
    /// `diagnostic`, where `finish_error_response` logs it under the
    /// request id. The secret store is the one this test was written for:
    /// its text names the master key file by absolute path, and a caller
    /// holding one environment's secret scope must not read it out of a
    /// 503. The legacy body still carries the sentence, byte for byte.
    #[test]
    fn every_backend_failure_is_opaque_and_keeps_its_text_for_the_log() {
        use crate::catalog::{
            BackendRestoreCoordinatorError, CatalogError, MigrationCoordinatorError,
        };
        use crate::command::ExecuteError;
        use crate::config_manager::PushError;

        let key_path = "cannot read secret_key_file /etc/deislet/master.key: No such file";
        let cases: Vec<(ApiError, &str)> = vec![
            (
                CatalogError::SecretsUnavailable(key_path.to_string()).into(),
                key_path,
            ),
            (
                CatalogError::Database(rusqlite::Error::QueryReturnedNoRows).into(),
                "Query returned no rows",
            ),
            (
                MigrationCoordinatorError::Backend {
                    backend: "postgres".to_string(),
                    tenant: "demo".to_string(),
                    message: "dial tcp 10.0.0.7:5432: connection refused".to_string(),
                }
                .into(),
                "10.0.0.7:5432",
            ),
            (
                BackendRestoreCoordinatorError::Unknown(
                    "dial tcp 10.0.0.9:9443: i/o timeout".to_string(),
                )
                .into(),
                "10.0.0.9:9443",
            ),
            (
                PushError::SendError("channel closed while sending to node-a".to_string()).into(),
                "node-a",
            ),
            (
                ExecuteError::SendError("channel closed while sending to node-a".to_string())
                    .into(),
                "node-a",
            ),
        ];
        for (error, backend_text) in cases {
            let detail = error.into_detail();
            assert!(
                detail.status.is_server_error(),
                "{backend_text}: {} is not a 5xx",
                detail.status
            );
            assert_eq!(
                detail.message, OPAQUE_MESSAGE,
                "the envelope message carries backend text for {backend_text}"
            );
            let diagnostic = detail
                .diagnostic
                .as_deref()
                .unwrap_or_else(|| panic!("{backend_text}: nothing left for the log"));
            assert!(
                diagnostic.contains(backend_text),
                "the log line lost {backend_text}: {diagnostic}"
            );
            let body = String::from_utf8(detail.structured_body(REQUEST)).unwrap();
            assert!(
                !body.contains(backend_text),
                "the structured envelope carries {backend_text}: {body}"
            );
        }
    }

    #[test]
    fn every_variant_maps_to_a_listed_code_and_its_status() {
        for (error, status, code) in every_variant() {
            assert_eq!(error.status(), status, "{error:?}");
            assert_eq!(error.code(), code, "{error:?}");
            assert!(codes::ALL.contains(&code), "{code} is not in codes::ALL");
        }
        for constructor in [
            ApiError::unauthenticated("x"),
            ApiError::insufficient_scope("apps:write", "x"),
            ApiError::route_not_found(),
            ApiError::payload_too_large("x"),
            ApiError::missing_header("idempotency-key"),
            ApiError::missing_header("If-Match"),
            ApiError::Conflict("x".into()).creating("application"),
            ApiError::BadRequest("x".into()).invalid_field("id"),
        ] {
            assert!(codes::ALL.contains(&constructor.code()), "{constructor:?}");
        }
    }

    /// The body is the envelope on every variant: one top-level key, the
    /// code the contract promises, the message the variant carries except
    /// where the text belongs to a backend.
    #[tokio::test]
    async fn the_body_is_one_error_object_with_the_promised_code() {
        for (error, status, code) in every_variant() {
            // The ones whose text belongs to a backend say only
            // [`OPAQUE_MESSAGE`]; the rest carry their own.
            let is_opaque = matches!(error, ApiError::InternalError(_) | ApiError::DatabaseError(_))
                || matches!(error, ApiError::Detailed(ref d) if d.diagnostic.is_some());
            let (got_status, bytes) = into_response_bytes(error).await;
            assert_eq!(got_status, status);
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body.as_object().unwrap().len(), 1, "only `error` at the top");
            assert_eq!(body["error"]["code"], code);
            let expected_message = if is_opaque {
                OPAQUE_MESSAGE
            } else {
                "x"
            };
            assert_eq!(body["error"]["message"], expected_message, "{code}");
            assert!(
                body["error"]["requestId"]
                    .as_str()
                    .unwrap()
                    .parse::<deis_wire::CanonicalId>()
                    .is_ok(),
                "the body carries an id until the renderer settles the real one"
            );
        }
        // Escaping and the `Json` extractor's compact form agree.
        let message = "quote \" and slash / and unicode é";
        let (_, bytes) = into_response_bytes(ApiError::BadRequest(message.to_string())).await;
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["message"], message);
        // The helpers that add a code, a target or a violation leave the
        // message exactly as the route wrote it.
        for (error, status, message) in [
            (
                ApiError::missing_header("idempotency-key"),
                StatusCode::BAD_REQUEST,
                "idempotency-key header is required",
            ),
            (
                ApiError::missing_header("If-Match"),
                StatusCode::BAD_REQUEST,
                "If-Match header is required",
            ),
            (
                ApiError::Conflict("x".into()).creating("application"),
                StatusCode::CONFLICT,
                "x",
            ),
            (
                ApiError::BadRequest("x".into()).invalid_field("id"),
                StatusCode::BAD_REQUEST,
                "x",
            ),
        ] {
            let (got_status, bytes) = into_response_bytes(error).await;
            assert_eq!(got_status, status);
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["message"], message);
        }
    }

    /// A message that starts with one of [`codes::MESSAGE_PREFIXES`] lifts
    /// the token into `code` and drops it from the message.
    #[test]
    fn the_envelope_lifts_a_code_prefix_out_of_the_message() {
        let message = format!(
            "{}: the Idempotency-Key was already used on route for x",
            codes::IDEMPOTENCY_KEY_REUSED
        );
        let error = ApiError::Conflict(message.clone());
        assert_eq!(error.code(), codes::IDEMPOTENCY_KEY_REUSED);
        assert_eq!(error.status(), StatusCode::CONFLICT);

        let body = structured(ApiError::Conflict(message));
        assert_eq!(body["error"]["code"], codes::IDEMPOTENCY_KEY_REUSED);
        assert_eq!(
            body["error"]["message"],
            "the Idempotency-Key was already used on route for x"
        );

        let in_progress = ApiError::Conflict(format!(
            "{}: still running",
            codes::IDEMPOTENCY_REQUEST_IN_PROGRESS
        ));
        let body = structured(in_progress);
        assert_eq!(body["error"]["retryable"], true);

        for code in [
            codes::PROVIDER_INSTALLATION_HAS_DEPENDENTS,
            codes::PROVIDER_INSTALLATION_IN_USE,
            codes::PROVIDER_TARGET_IN_USE,
            codes::PROVIDER_RESOURCE_DELETED,
        ] {
            let error = ApiError::Conflict(format!("{code}: detail"));
            assert_eq!(error.code(), code);
        }
        // A token without the separator is just a message.
        let error = ApiError::Conflict(format!("{}x", codes::PROVIDER_TARGET_IN_USE));
        assert_eq!(error.code(), codes::CONFLICT);
    }

    /// Every variant's envelope carries the members the contract requires
    /// and no member it does not declare; the message is the variant's own
    /// except where the text belongs to a backend.
    #[test]
    fn the_v1_envelope_has_the_promised_shape_per_code() {
        let contract = read_json(&contracts_dir().join("control-error.schema.json"));
        let declared: BTreeSet<&str> = contract["properties"]["error"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let required: BTreeSet<&str> = contract["properties"]["error"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| member.as_str().unwrap())
            .collect();
        for (error, _, code) in every_variant() {
            let detail = error.into_detail();
            let expected_message = if detail.diagnostic.is_some() {
                OPAQUE_MESSAGE
            } else {
                "x"
            };
            let body: Value = serde_json::from_slice(&detail.structured_body(REQUEST)).unwrap();
            let object = body["error"].as_object().expect("an object under error");
            assert_eq!(object["code"], code);
            assert_eq!(object["message"], expected_message, "{code}");
            assert_eq!(object["requestId"], REQUEST);
            assert_eq!(object["violations"], json!([]));
            assert_eq!(object["retryable"], codes::is_retryable(code));
            assert!(!object.contains_key("target"));
            assert!(!object.contains_key("retryAfterSeconds"));
            assert!(!object.contains_key("docs"));
            assert!(!object.contains_key("provider"));
            assert_eq!(
                body.as_object().unwrap().len(),
                1,
                "only `error` at the top"
            );
            let members: BTreeSet<&str> = object.keys().map(String::as_str).collect();
            assert!(
                members.is_subset(&declared),
                "{code} carries a member the contract does not declare: {:?}",
                members.difference(&declared).collect::<Vec<_>>()
            );
            assert!(
                required.is_subset(&members),
                "{code} lacks a member the contract requires: {:?}",
                required.difference(&members).collect::<Vec<_>>()
            );
        }
    }

    /// The contract's positive fixtures are what the builders produce, so
    /// the two documents cannot drift on any member: a fixture the checker
    /// validates against the schema is a body this server actually sends.
    #[test]
    fn the_structured_bodies_round_trip_the_contract_fixtures() {
        let fixtures = contracts_dir().join("fixtures/valid");
        let cases: Vec<(&str, ApiError)> = vec![
            (
                "control-error.json",
                ApiError::NotFound(
                    "Application 0199b603-0e48-733c-9864-4c9dd92d5328 does not exist".to_string(),
                ),
            ),
            (
                "control-error-full.json",
                ApiError::BadRequest(
                    "The provider target configuration was rejected before any provider \
                     resource was created"
                        .to_string(),
                )
                .with_target("provider-target", Some("config.zoneId".to_string()))
                .with_violations(vec![
                    Violation {
                        field: "config.accountId".to_string(),
                        code: "required",
                        message: "The account owning the zone must be declared explicitly"
                            .to_string(),
                    },
                    Violation {
                        field: "config.zoneId".to_string(),
                        code: "unknown-reference",
                        message: "The zone is not visible to the bound provider installation \
                                  credential"
                            .to_string(),
                    },
                ])
                .with_docs("https://deislet.dev/docs/api/v1/errors#invalid-argument")
                .with_provider(provider_diagnostic()),
            ),
            (
                "control-error-retryable.json",
                ApiError::coded(
                    StatusCode::BAD_GATEWAY,
                    codes::BACKEND_UNAVAILABLE,
                    "The provider driver could not reach the backend management API; no \
                     deployment state changed",
                )
                .with_target("provider-installation", None)
                .retry_after(30)
                .with_docs("https://deislet.dev/docs/api/v1/errors#backend-unavailable"),
            ),
        ];
        for (file, error) in cases {
            let expected = read_json(&fixtures.join(file));
            let request_id = expected["error"]["requestId"].as_str().unwrap();
            let actual: Value =
                serde_json::from_slice(&error.into_detail().structured_body(request_id)).unwrap();
            assert_eq!(actual, expected, "{file}");
        }
    }

    #[tokio::test]
    async fn builders_enrich_the_envelope() {
        let make = || {
            ApiError::BadRequest("limit must be between 1 and 1000".to_string())
                .with_code(codes::LIMIT_OUT_OF_RANGE)
                .with_target("query", Some("limit".to_string()))
                .with_violations(vec![Violation {
                    field: "limit".to_string(),
                    code: codes::LIMIT_OUT_OF_RANGE,
                    message: "1001 is above 1000".to_string(),
                }])
                .with_docs("https://example.invalid/pagination")
                .with_provider(provider_diagnostic())
        };
        let (status, bytes) = into_response_bytes(make()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], codes::LIMIT_OUT_OF_RANGE);
        assert_eq!(body["error"]["message"], "limit must be between 1 and 1000");
        assert_eq!(body["error"]["retryable"], false);
        assert!(
            body["error"]["requestId"]
                .as_str()
                .unwrap()
                .parse::<deis_wire::CanonicalId>()
                .is_ok(),
            "the body carries an id until the renderer settles the real one"
        );
        let body = structured(make());
        assert_eq!(
            body,
            json!({
                "error": {
                    "code": "limit-out-of-range",
                    "message": "limit must be between 1 and 1000",
                    "requestId": REQUEST,
                    "target": { "resource": "query", "field": "limit" },
                    "violations": [
                        { "field": "limit", "code": "limit-out-of-range", "message": "1001 is above 1000" }
                    ],
                    "retryable": false,
                    "docs": "https://example.invalid/pagination",
                    "provider": {
                        "backendId": "cloudflare-workers-pages",
                        "category": "configuration",
                        "code": "ZONE_NOT_VISIBLE",
                        "sideEffect": "none",
                        "requestId": "0cb2a4e1-4f2a-4c47-9e3b-4a2d0c5f7ab1"
                    }
                }
            })
        );
    }

    /// `retryable` is the code's, never the request's: a hint rides only on
    /// a retryable code, and re-coding drops a hint that no longer applies.
    #[test]
    fn a_retry_hint_follows_the_code() {
        let body = structured(ApiError::ServiceUnavailable("x".to_string()).retry_after(30));
        assert_eq!(body["error"]["retryable"], true);
        assert_eq!(body["error"]["retryAfterSeconds"], 30);

        let recoded = ApiError::ServiceUnavailable("x".to_string())
            .retry_after(30)
            .with_code(codes::NOT_CONFIGURED);
        let body = structured(recoded);
        assert_eq!(body["error"]["code"], codes::NOT_CONFIGURED);
        assert_eq!(body["error"]["retryable"], false);
        assert!(
            !body["error"]
                .as_object()
                .unwrap()
                .contains_key("retryAfterSeconds")
        );

        let promoted = ApiError::Conflict("x".to_string()).with_code(codes::BACKEND_UNAVAILABLE);
        assert_eq!(structured(promoted)["error"]["retryable"], true);
    }

    #[test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "the hint is refused in debug builds only"
    )]
    #[should_panic(expected = "a retry hint on the non-retryable code not-found")]
    fn a_retry_hint_on_a_non_retryable_code_is_a_programming_error() {
        let _ = ApiError::NotFound("x".to_string()).retry_after(1);
    }

    /// The backend's text stays in the diagnostic for the log; the envelope
    /// says only that the control plane failed.
    #[tokio::test]
    async fn backend_text_is_opaque_on_the_envelope() {
        let detail = ApiError::DatabaseError("locked".to_string()).into_detail();
        assert_eq!(detail.message, OPAQUE_MESSAGE);
        assert_eq!(detail.diagnostic.as_deref(), Some("locked"));
        let body = structured(ApiError::DatabaseError("locked".to_string()));
        assert_eq!(body["error"]["message"], OPAQUE_MESSAGE);
        assert_eq!(body["error"]["code"], codes::DATABASE_UNAVAILABLE);

        let path = "/var/lib/deis/bundles/x: permission denied";
        let detail = ApiError::InternalError(path.to_string()).into_detail();
        assert_eq!(detail.message, OPAQUE_MESSAGE);
        assert_eq!(detail.diagnostic.as_deref(), Some(path));
        assert!(
            !String::from_utf8(detail.structured_body(REQUEST))
                .unwrap()
                .contains("/var/lib"),
            "the envelope must not carry the path"
        );
        let (status, bytes) = into_response_bytes(ApiError::InternalError(path.to_string())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("/var/lib"),
            "the rendered body must not carry the path: {text}"
        );

        // A sentence a handler wrote itself is not a backend's and stays.
        let detail = ApiError::coded(
            StatusCode::BAD_GATEWAY,
            codes::BACKEND_UNAVAILABLE,
            "node n1 did not answer",
        )
        .into_detail();
        assert_eq!(detail.message, "node n1 did not answer");
        assert!(detail.diagnostic.is_none());
    }

    /// The producers that build their answer outside every mount - the
    /// authorization gate, the route fallback - render the same envelope,
    /// and the ids the caller is to quote are settled from what it sent.
    #[tokio::test]
    async fn out_of_mount_producers_render_the_envelope() {
        use crate::api::versioning::RequestId;
        let context = RequestContext::of(RequestId(REQUEST.parse().unwrap()));
        let body_of = |error: ApiError| {
            let response =
                error.into_response_for(ApiSurface::of_path("/api/v1/apps"), context);
            let status = response.status();
            // A producer outside every mount echoes both ids, so a refusal
            // can be tied to the calls the caller sent around it.
            assert_eq!(
                response.headers()[crate::api::versioning::REQUEST_ID_HEADER],
                REQUEST
            );
            assert_eq!(
                response.headers()[crate::api::versioning::CORRELATION_ID_HEADER],
                REQUEST
            );
            (status, response)
        };

        let (status, response) = body_of(ApiError::unauthenticated("no credential"));
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], codes::UNAUTHENTICATED);
        assert_eq!(body["error"]["message"], "no credential");
        assert_eq!(body["error"]["requestId"], REQUEST);

        let (status, response) =
            body_of(ApiError::insufficient_scope("apps:write", "create app"));
        assert_eq!(status, StatusCode::FORBIDDEN);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], codes::INSUFFICIENT_SCOPE);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("create app."),
            "the sentence says what the call needed: {body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("apps:write"),
            "the hint names the scope that would be allowed: {body}"
        );

        let (status, response) = body_of(ApiError::route_not_found());
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], codes::ROUTE_NOT_FOUND);
    }

    #[tokio::test]
    async fn into_response_for_renders_the_envelope_and_echoes_both_ids() {
        use crate::api::versioning::RequestId;
        let context = RequestContext::of(RequestId(REQUEST.parse().unwrap()));
        // A path that names no surface of its own gets the same envelope.
        for path in ["/api/apps", "/api/v1/apps", "/api/platform/nope"] {
            let response = ApiError::unauthenticated("x")
                .into_response_for(ApiSurface::of_path(path), context);
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers()[crate::api::versioning::REQUEST_ID_HEADER],
                REQUEST
            );
            assert_eq!(
                response.headers()[crate::api::versioning::CORRELATION_ID_HEADER],
                REQUEST
            );
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["code"], codes::UNAUTHENTICATED, "{path}");
            assert_eq!(body["error"]["requestId"], REQUEST, "{path}");
        }

        let response = ApiError::route_not_found().into_response_for(
            ApiSurface::of_path("/api/platform/nope"),
            RequestContext::resolve(
                &axum::http::Extensions::new(),
                &axum::http::HeaderMap::new(),
            ),
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(
            body["error"]["requestId"]
                .as_str()
                .unwrap()
                .parse::<deis_wire::CanonicalId>()
                .is_ok(),
            "an id is minted when none was settled"
        );
    }

    #[test]
    fn node_errors_keep_their_display_for_the_log() {
        use crate::runtime_client::NodeError;
        let unknown = ApiError::from(NodeError::UnknownNode);
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        let transport = ApiError::from(NodeError::Transport(
            "connect http://10.0.0.7:50051: refused".to_string(),
        ));
        assert_eq!(transport.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(transport.code(), codes::BACKEND_UNAVAILABLE);
        let detail = transport.into_detail();
        // The node's management address is for the log, not the caller.
        assert_eq!(detail.message, OPAQUE_MESSAGE);
        assert_eq!(
            detail.diagnostic.as_deref(),
            Some("transport error: connect http://10.0.0.7:50051: refused")
        );
        assert!(detail.retryable);
        let rpc = ApiError::from(NodeError::Rpc {
            code: tonic::Code::Unavailable,
            message: "down".to_string(),
        });
        assert_eq!(rpc.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn coordinator_backend_failures_name_the_backend_code() {
        use crate::catalog::{BackendRestoreCoordinatorError, MigrationCoordinatorError};
        let error = ApiError::from(MigrationCoordinatorError::Backend {
            backend: "deis-store".to_string(),
            tenant: "t".to_string(),
            message: "gone".to_string(),
        });
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code(), codes::BACKEND_UNAVAILABLE);
        let detail = error.into_detail();
        assert_eq!(detail.message, OPAQUE_MESSAGE);
        assert!(
            detail
                .diagnostic
                .as_deref()
                .is_some_and(|text| text.contains("deis-store")),
            "the backend's name is for the log"
        );
        let error = ApiError::from(BackendRestoreCoordinatorError::Unknown("?".to_string()));
        assert_eq!(error.code(), codes::BACKEND_UNAVAILABLE);
        assert_eq!(error.into_detail().message, OPAQUE_MESSAGE);
        let error = ApiError::from(MigrationCoordinatorError::Configuration("k".to_string()));
        assert_eq!(error.code(), codes::NOT_CONFIGURED);
    }
}
