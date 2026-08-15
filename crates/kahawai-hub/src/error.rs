//! One shape for every refusal the API makes.
//!
//! Errors used to be `(StatusCode, String)`, and twenty of those strings were
//! `format!("{e:#}")` — the whole anyhow chain. A viewer whose transcode
//! failed was shown the hub's scratch layout, the pipeline worker's argv and
//! GStreamer's stderr, because the web UI renders hub errors on purpose: they
//! are usually the only clue anyone gets. The fix belongs here, at the source,
//! and `item_artwork` had already found it — log the chain, return a fixed
//! string, citing SEC-WEB-7.
//!
//! The other half is that the difference between refusals lived only in
//! English. `too many concurrent streams` and `this item has no playable
//! source` were both 409, one of which clears the moment a session ends and
//! one of which is forever, and no client may branch on prose. So every error
//! carries a `code`:
//!
//! ```json
//! {"code": "session_cap", "message": "too many concurrent streams; close one first"}
//! ```
//!
//! `code` is enumerated, published in the OpenAPI schema, and stable.
//! `message` is for a person and its wording is not contractual.
//!
//! **The status says whether to retry; the code says what happened.** 429 and
//! 503 mean the same request may work later — most clear on their own, and
//! two (`setup_required`, `provider_unconfigured`) clear when an operator acts
//! — 5xx is worth a backoff, and every other 4xx is final.
//! `Retry-After` says WHEN, on the refusals where the hub knows — the login
//! lockout does, and the stream cap does not, because it clears when a person
//! stops watching something.
//! That split is HTTP's own and needs no kahawai-specific knowledge — which is
//! the point, because a third-party client (HUB-28) gets the retry decision
//! right without a table of our codes compiled into it. There is deliberately
//! no `retryable` field: it would be the same decision computed in two places,
//! free to disagree.
//!
//! Two responses are deliberately outside all of this. Item artwork's 404 is
//! an `ApiErrorBody` like the rest but is CACHEABLE — see its handler — and
//! `stream_session`'s 416 has no body at all, which is what RFC 9110 asks for:
//! the answer is in `Content-Range`, and there is nothing a code would add.
//!
//! Each code maps to exactly one status, in `ErrorCode::status`. That is the
//! whole reason to have codes rather than statuses with prose — a status can
//! mean several things, a code means one — and it is why a construction site
//! picks a code and never a status.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// What went wrong, as something a client can branch on.
///
/// Adding a variant is how a new refusal becomes visible to clients; reusing
/// a near-enough one is how it stays invisible. Prefer a new variant.
/// `Deserialize` for one reader: the test that walks the published document
/// and checks each declared status against the code its description names. A
/// declaration drifting from `status()` is the one class of untruth the
/// TypeScript contract test cannot see, and it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The hub failed at something that should have worked. Nothing about the
    /// request is wrong; the detail is in the hub's log, deliberately not here.
    Internal,
    /// The request is malformed or asks for something impossible.
    BadRequest,
    /// A QUERY without `Content-Type: application/json`.
    UnsupportedMediaType,
    /// A verb this path does not answer. `Allow` names the ones it does.
    MethodNotAllowed,
    /// The body is larger than the hub will buffer. A batch to split up, not
    /// a body to fix — which is why it is not `BadRequest`.
    PayloadTooLarge,
    /// An image subtitle track asked for in a text form it has none of.
    UnsupportedTrack,
    /// No credentials, or credentials that are no longer good. Distinct from
    /// `Forbidden`: re-authenticating is the way out of this one.
    Unauthenticated,
    /// The username and password do not match an account.
    InvalidCredentials,
    /// The refresh token is unknown, expired or already spent.
    InvalidRefresh,
    /// Too many failed sign-ins from here. Clears on its own — the message
    /// says roughly when.
    LoginThrottled,
    /// Authenticated, and not allowed this. Re-authenticating changes nothing;
    /// a different account might.
    Forbidden,
    /// Authenticated, and not an administrator.
    AdminRequired,
    /// No such thing, or none this account may see. AUTH-11 keeps those two
    /// deliberately indistinguishable — a session id is not a capability.
    NotFound,
    /// The request is fine and the current state refuses it.
    Conflict,
    /// It would leave the hub with no administrator. HUB-10 refuses both the
    /// demotion and the deletion, so nothing can lock an operator out.
    LastAdmin,
    /// Somebody else wrote to this since it was read (UI-25). The request was
    /// well formed and would have silently discarded their change; reading the
    /// current state and deciding again is the way out, which is why it is not
    /// a plain `Conflict` — a client can act on this one on its own.
    StaleWrite,
    /// You cannot do this to your own account. Deliberately not `Forbidden`,
    /// which is what an unprivileged token gets: the way out of this one is a
    /// different target, not a different session.
    SelfTarget,
    /// The hub has no administrator yet; everything else waits for setup.
    SetupRequired,
    /// Setup has already run. It is a first-run flow and does not repeat.
    SetupComplete,
    /// A metadata or subtitle provider needs credentials this deployment has
    /// not been given. The request is well formed and the fix is on the
    /// server, which is why it is not a `BadRequest`: a client told its
    /// request was wrong has nowhere useful to go with that, and the two
    /// routes that DO take a provider key deliberately answer `BadRequest`
    /// for a blank field, which is the distinction this keeps.
    ProviderUnconfigured,
    /// A provider was asked and did not answer usefully. Upstream, not us.
    ProviderError,
    /// The subtitle download entitlement is spent — this account's, or the
    /// server's shared anonymous one. Not an outage: the way out is an
    /// account or tomorrow, and the message says which. Deliberately not 429,
    /// which invites a retry — this one clears in hours, not seconds.
    SubtitleQuotaSpent,
    /// This item cannot be played, and asking again will not change that:
    /// no sources, or nothing about it that this client can be served.
    Unplayable,
    /// A satellite that should answer did not — not connected, or connected
    /// and silent past a deadline. Distinct from `SourceOffline`, which is
    /// specifically the mediahost holding a file a viewer wants to play and
    /// which playback clients branch on; this one is the admin-side answer
    /// for a box that is simply not talking.
    SatelliteUnreachable,
    /// The bytes live on a mediahost that is not connected right now. Nothing
    /// is wrong with the item or the request — stand by and ask again.
    SourceOffline,
    /// This account already holds as many playback sessions as it may
    /// (HUB `max_sessions_per_user`). Clears as soon as one ends, which is why
    /// it is not a `Conflict`: a client playing a queue must wait, not give up.
    SessionCap,
}

impl ErrorCode {
    /// The one status this code is returned with. A code means one thing, so
    /// it can only mean one status; keeping the table here is what stops a
    /// call site from pairing them differently on a Friday.
    pub fn status(self) -> StatusCode {
        use ErrorCode::*;
        match self {
            Internal => StatusCode::INTERNAL_SERVER_ERROR,
            BadRequest => StatusCode::BAD_REQUEST,
            UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            UnsupportedTrack => StatusCode::UNPROCESSABLE_ENTITY,
            Unauthenticated | InvalidCredentials | InvalidRefresh => StatusCode::UNAUTHORIZED,
            LoginThrottled | SessionCap => StatusCode::TOO_MANY_REQUESTS,
            Forbidden | AdminRequired => StatusCode::FORBIDDEN,
            NotFound => StatusCode::NOT_FOUND,
            Conflict | SetupComplete | Unplayable | LastAdmin | SelfTarget | StaleWrite
            | SubtitleQuotaSpent => StatusCode::CONFLICT,
            SetupRequired | SourceOffline | SatelliteUnreachable | ProviderUnconfigured => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ProviderError => StatusCode::BAD_GATEWAY,
        }
    }
}

/// The body every 4xx and 5xx carries. Named for the schema: this is what a
/// client parses, and `ApiError` is what the hub throws around internally.
#[derive(Debug, Serialize, ToSchema)]
#[schema(as = ApiErrorBody)]
pub struct ApiErrorBody {
    pub code: ErrorCode,
    /// For a person. Never a chain, never a path, never a subprocess's stderr.
    pub message: String,
}

#[derive(Debug)]
pub struct ApiError {
    body: ApiErrorBody,
    /// Seconds, for `Retry-After`, when the hub actually knows.
    ///
    /// The contract here is that the STATUS says whether to retry, which
    /// leaves a client that honours it knowing to come back and not when. The
    /// login lockout ranges from 30 s to 15 minutes and the only statement of
    /// which was in `message` — prose this module tells clients not to read.
    ///
    /// Absent where nobody knows: `session_cap` clears when a person stops
    /// watching something, and a number there would be an invention.
    retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            body: ApiErrorBody {
                code,
                message: message.into(),
            },
            retry_after: None,
        }
    }

    /// How long until this is worth trying again, when that is a fact rather
    /// than a guess. Sent as `Retry-After`.
    pub fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }

    /// A failure with a cause worth keeping: the chain goes to the log, and the
    /// client gets the sentence the CALLER wrote.
    ///
    /// The message is a parameter and not `error.to_string()`, which was the
    /// first cut of this and did not hold. `to_string()` is the outermost
    /// anyhow layer, and an outermost layer is not a boundary — it is whatever
    /// the producer put there. Two live counter-examples, both on the path this
    /// module exists for:
    ///
    /// - `sessions.rs` bails with `pipeline worker exited at start ({status}):
    ///   {tail}`, where `tail` is four lines of the worker's log: GStreamer
    ///   stderr and panic messages naming source files. The chain is one layer
    ///   deep and the layer IS the leak.
    /// - the fallback-sink path wraps with `with_context(|| format!("first
    ///   attempt: {first:#}"))`, which flattens an entire chain — scratch dir,
    ///   worker executable path — into a single layer's message.
    ///
    /// So no error's own text crosses this boundary. What a client reads is
    /// written at the call site, for a reader, and `ApiErrorBody`'s promise
    /// that a message is never a chain is a property of this signature rather
    /// than a hope about every producer upstream.
    pub fn log(
        code: ErrorCode,
        message: impl Into<String>,
        error: impl Into<anyhow::Error>,
    ) -> Self {
        let error = error.into();
        tracing::warn!(code = ?code, error = format!("{error:#}"), "request refused");
        Self::new(code, message)
    }
}

impl ApiError {
    /// The code, for a log line that has to name the refusal without repeating
    /// its cause. `seek_session` uses it: the chain is logged at debug and the
    /// warn line says which of the three answers went out.
    pub fn code(&self) -> ErrorCode {
        self.body.code
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.body.code.status();
        let mut response = (status, axum::Json(self.body)).into_response();
        if let Some(secs) = self.retry_after {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(secs),
            );
        }
        response
    }
}

/// `Json`, refusing in the API's own shape.
///
/// axum's own rejection is `text/plain` with no code, so a malformed body was
/// the one refusal in the hub that did not carry one — on twenty-one routes
/// whose OpenAPI says every 4xx is an `ApiErrorBody`. Three routes already
/// took `Result<Json<_>, JsonRejection>` by hand to avoid exactly that; this
/// makes it the default rather than a thing each route has to remember.
///
/// Two answers, because axum already knows which is which and throwing that
/// away would be against the grain of everything else here: a wrong or missing
/// `Content-Type` is 415 `unsupported_media_type`, and a body that will not
/// parse or is the wrong shape is 400 `bad_request` (axum distinguishes those
/// two as 400 and 422; the difference is not one a client acts on, and
/// `message` carries its account of which it was).
///
/// A client sending `text/plain` has a different bug from one sending broken
/// JSON, and an earlier cut of this collapsed both into 400 — which quietly
/// changed the status QUERY had been answering for a wrong content type, and
/// its test said so.
///
/// Every route taking a body declares both now. Eleven declared no 400 at all
/// before this: a document that omits the response a malformed body produces
/// is not one a client can be generated from, which is HUB-28's whole point.
///
/// `item_query` keeps its own 415 as well, for the case that is not a
/// rejection at all — a QUERY arriving with no body, which axum reports as an
/// absent `Option` rather than an error.
pub struct ApiJson<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejection_error(rejection.status(), rejection.body_text())),
        }
    }
}

/// An axum rejection, in this API's shape.
///
/// The code comes from the status axum already chose, rather than from a match
/// on its variants. Enumerating them is how the first cut got this wrong twice
/// over: a body past `DefaultBodyLimit` is a 413 and became a 400, so a client
/// could not tell "send a smaller batch" from "your JSON is broken"; and two
/// `PathRejection` variants — a missing path param, a tuple whose arity does
/// not match the route — are 500s, because they mean the hub's own routing and
/// handler signature disagree. Reporting those as 400 blames the caller for
/// our bug and keeps them out of an operator's 5xx alerting.
///
/// Every extractor here goes through this, so they cannot drift apart on it.
fn rejection_error(status: StatusCode, body_text: String) -> ApiError {
    let code = match status {
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ErrorCode::UnsupportedMediaType,
        StatusCode::PAYLOAD_TOO_LARGE => ErrorCode::PayloadTooLarge,
        // Ours, not theirs. `internal` logs and answers a fixed sentence
        // exactly because a caller can do nothing with the detail.
        s if s.is_server_error() => {
            tracing::error!(status = %s, detail = %body_text, "extractor failed");
            return ApiError::new(
                ErrorCode::Internal,
                "the hub could not complete this request",
            );
        }
        _ => ErrorCode::BadRequest,
    };
    ApiError::new(code, body_text)
}

/// The optional half, for the one route whose body may legitimately be absent.
///
/// `Option<Json<T>>` is not the same as "a body, or nothing": axum yields
/// `None` only when `Content-Type` is missing entirely, and answers its own
/// `text/plain` rejection for a present body that will not parse. So QUERY on
/// an item — the last route on a bare `Json` — was still handing out a 400
/// with no code, which is the exact hole this extractor exists to close.
impl<S, T> axum::extract::OptionalFromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(
        req: axum::extract::Request,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        match <axum::Json<T> as axum::extract::OptionalFromRequest<S>>::from_request(req, state)
            .await
        {
            Ok(Some(axum::Json(value))) => Ok(Some(Self(value))),
            Ok(None) => Ok(None),
            Err(rejection) => Err(rejection_error(rejection.status(), rejection.body_text())),
        }
    }
}

/// `Query`, refusing in the API's own shape — the same reason as `ApiJson`.
///
/// `?limit=abc` was answering axum's `text/plain` with no code, on routes that
/// declared no 400 at all. Closing the body hole and leaving this one open
/// would have made the contract true of most refusals, which is not what the
/// document says.
pub struct ApiQuery<T>(pub T);

impl<S, T> axum::extract::FromRequestParts<S> for ApiQuery<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejection_error(rejection.status(), rejection.body_text())),
        }
    }
}

/// `Path`, likewise. Only the typed ones can reject — a `Path<String>` takes
/// whatever the router matched — but they all go through this, because a rule
/// about which segments happen to be parsed today is a rule somebody has to
/// remember when they add the next one.
pub struct ApiPath<T>(pub T);

impl<S, T> axum::extract::FromRequestParts<S> for ApiPath<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejection_error(rejection.status(), rejection.body_text())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code answers with the status its meaning implies, and the two
    /// retryable classes are spelled the way HTTP spells them — a client that
    /// knows nothing about kahawai still waits on the right ones.
    #[test]
    fn transient_refusals_are_the_transient_statuses() {
        for code in [ErrorCode::SourceOffline, ErrorCode::SetupRequired] {
            assert_eq!(code.status(), StatusCode::SERVICE_UNAVAILABLE, "{code:?}");
        }
        for code in [ErrorCode::SessionCap, ErrorCode::LoginThrottled] {
            assert_eq!(code.status(), StatusCode::TOO_MANY_REQUESTS, "{code:?}");
        }
        // The pair this whole change exists for: one clears when a session
        // ends, one never does, and they used to be the same 409.
        assert_ne!(
            ErrorCode::SessionCap.status(),
            ErrorCode::Unplayable.status()
        );
    }

    /// The property `ApiError::log`'s signature exists to hold, and the one an
    /// earlier cut of it did not: the error's own text never reaches a client.
    ///
    /// Both shapes here are real. `sessions.rs` bails with the worker's stderr
    /// inside its outermost message, and the fallback-sink path flattens a
    /// whole chain into one layer with `with_context(|| format!("{first:#}"))`
    /// — so `to_string()`, which reads exactly that layer, leaked in both.
    #[test]
    fn no_part_of_an_error_chain_reaches_the_client() {
        let leaky = anyhow::anyhow!(
            "pipeline worker exited at start (signal: 11): /usr/lib/gstreamer-1.0/libgstx264.so: assertion failed"
        );
        let flattened =
            anyhow::anyhow!("creating /var/lib/kahawai/scratch/01J9: permission denied")
                .context("first attempt: spawning worker /usr/local/bin/kahawai");

        for error in [leaky, flattened] {
            let refused = ApiError::log(ErrorCode::Unplayable, "this item cannot be played", error);
            assert_eq!(refused.body.message, "this item cannot be played");
            for leak in ["gstreamer", "scratch", "assertion", "/usr", "/var"] {
                assert!(
                    !refused.body.message.to_lowercase().contains(leak),
                    "{leak:?} reached the client: {}",
                    refused.body.message
                );
            }
        }
    }

    /// Not every producer's sentence is a leak, and collapsing them all was
    /// its own regression.
    ///
    /// The subtitle entitlement runs out five downloads into an anonymous day.
    /// Its message is authored, names the way out — add an account, or wait
    /// until tomorrow — and leaks nothing, and folding it into "the provider
    /// did not answer" sent people to retry an outage that was not happening.
    /// A typed error is what lets the API tell the two apart, and reading THAT
    /// type's `Display` rather than the chain around it is what keeps it safe.
    #[test]
    fn an_authored_refusal_keeps_its_sentence() {
        let spent = crate::opensubtitles::QuotaSpent(
            "OpenSubtitles download quota exhausted (anonymous: 5 per 24 h; add an account \
             for more) — it resets 24 h after your first download today"
                .into(),
        );
        let refused = ApiError::new(ErrorCode::SubtitleQuotaSpent, spent.to_string());
        assert!(
            refused.body.message.contains("add an account"),
            "{}",
            refused.body.message
        );
        // And it is not an upstream fault, which is the read that made a
        // spent budget look like something to retry.
        assert_ne!(
            ErrorCode::SubtitleQuotaSpent.status(),
            ErrorCode::ProviderError.status()
        );
    }

    #[test]
    fn the_body_is_json_with_a_snake_case_code() {
        let body = serde_json::to_string(&ApiErrorBody {
            code: ErrorCode::SourceOffline,
            message: "the host holding this file is not connected".into(),
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"code":"source_offline","message":"the host holding this file is not connected"}"#
        );
    }
}
