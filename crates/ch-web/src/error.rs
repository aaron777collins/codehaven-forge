//! Errors as pages. A service error becomes a small, plain HTML page with the right status.
//! Messages are written for people and never include SQL, paths or stack traces.

use crate::assets;
use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ch_core::Error;

pub type WebResult<T> = Result<T, WebError>;

#[derive(Debug)]
pub enum WebError {
    Core(Error),
    /// The form's token or origin did not check out.
    Csrf,
    /// The request itself was malformed.
    Bad(String),
}

impl From<Error> for WebError {
    fn from(e: Error) -> Self {
        WebError::Core(e)
    }
}

impl WebError {
    pub fn not_found() -> Self {
        WebError::Core(Error::NotFound)
    }
    pub fn bad(msg: impl Into<String>) -> Self {
        WebError::Bad(msg.into())
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    css: String,
    status: u16,
    heading: String,
    message: String,
    hint: String,
}

fn page(status: StatusCode, heading: &str, message: &str, hint: &str) -> Response {
    let t = ErrorPage {
        css: assets::url("app.css"),
        status: status.as_u16(),
        heading: heading.into(),
        message: message.into(),
        hint: hint.into(),
    };
    let body = t.render().unwrap_or_else(|_| format!("{heading}\n{message}\n"));
    let mut r = (status, body).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        match self {
            WebError::Csrf => page(
                StatusCode::FORBIDDEN,
                "That form has expired",
                "The page you submitted was opened a while ago or from somewhere else, so it was not accepted.",
                "Go back, reload the page and try again.",
            ),
            WebError::Bad(m) => page(StatusCode::BAD_REQUEST, "That did not look right", &m, "Go back and check what you entered."),
            WebError::Core(e) => match e {
                Error::NotFound => page(
                    StatusCode::NOT_FOUND,
                    "Not found",
                    "There is nothing here, or you may not have access to it.",
                    "Check the address, or go back to the start page.",
                ),
                Error::Unauthorized(_) => {
                    let mut r = StatusCode::SEE_OTHER.into_response();
                    r.headers_mut().insert(header::LOCATION, HeaderValue::from_static("/login"));
                    r
                }
                Error::Forbidden(m) => {
                    page(StatusCode::FORBIDDEN, "You cannot do that", &m, "Ask someone with more access if you need this.")
                }
                Error::Validation(errs) => {
                    let msg = errs
                        .iter()
                        .map(|e| e.message.clone().unwrap_or_else(|| format!("{} {}", e.field, e.code)))
                        .collect::<Vec<_>>()
                        .join(" ");
                    page(StatusCode::UNPROCESSABLE_ENTITY, "Please check that", &msg, "Go back and correct it.")
                }
                Error::Conflict(m) => page(StatusCode::CONFLICT, "That conflicts with something", &m, "Reload the page and try again."),
                Error::MergeBlocked(reasons) => {
                    page(StatusCode::METHOD_NOT_ALLOWED, "This cannot be merged yet", &reasons.join(" "), "Go back to the pull request.")
                }
                Error::RateLimited { retry_after_secs } => {
                    let mins = retry_after_secs.div_ceil(60).max(1);
                    let mut r = page(
                        StatusCode::TOO_MANY_REQUESTS,
                        "Slow down a little",
                        &format!(
                            "There have been too many requests. Try again in about {mins} minute{}.",
                            if mins == 1 { "" } else { "s" }
                        ),
                        "",
                    );
                    if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                        r.headers_mut().insert(header::RETRY_AFTER, v);
                    }
                    r
                }
                Error::Unavailable(_) => page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Not available right now",
                    "This part of Codehaven is not ready yet.",
                    "Try again in a moment.",
                ),
                Error::Internal(err) => {
                    tracing::error!(error = %err, "page failed");
                    page(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Something went wrong",
                        "That is on us, not you.",
                        "Try again, and tell the server administrator if it keeps happening.",
                    )
                }
            },
        }
    }
}
