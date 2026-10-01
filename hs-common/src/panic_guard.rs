//! Turning a panic into a value at the boundaries where one request or one
//! event must not take the process (or a task that owes an ack) down.
//!
//! This only works because the workspace builds with `panic = "unwind"`: under
//! `panic = "abort"` the first panic on untrusted input ended the whole
//! daemon before any of these could run (RA-5).

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;

/// The text of a panic payload (`panic!("..")` and `panic!("{}", x)` carry a
/// `&str` / `String`; anything else is reported as such).
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with a non-string payload".to_string()
    }
}

/// Await `fut`; a panic while polling it is `Err(message)` instead of
/// unwinding into the caller.
///
/// `fut` is wrapped in `AssertUnwindSafe`: callers use this at a boundary
/// that discards everything the future owned (the request or event it was
/// serving), so no half-updated state of it is observed afterwards.
pub async fn catch_panic<F: Future>(fut: F) -> Result<F::Output, String> {
    AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .map_err(|payload| panic_message(payload.as_ref()))
}

/// Axum middleware: a panic in any handler below it becomes a 500 with no
/// internals in the body, logged at ERROR with the route, and the server
/// keeps serving.
///
/// ```ignore
/// let app = Router::new()
///     // …routes…
///     .layer(axum::middleware::from_fn(hs_common::panic_guard::http::catch_panic));
/// ```
///
/// Apply it as the outermost layer so it also covers the other layers'
/// handlers. It cannot catch a panic on a task the handler spawned (that
/// task simply ends; tokio does not propagate it).
#[cfg(feature = "catch-panic")]
pub mod http {
    use axum::extract::{MatchedPath, Request};
    use axum::http::{header, StatusCode};
    use axum::middleware::Next;
    use axum::response::{IntoResponse, Response};

    pub async fn catch_panic(request: Request, next: Next) -> Response {
        let method = request.method().clone();
        // The route template, not the raw path: no ids or secrets in the log.
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "<unmatched>".to_string());
        match super::catch_panic(next.run(request)).await {
            Ok(response) => response,
            Err(message) => {
                tracing::error!(
                    method = %method,
                    route = %route,
                    panic = %message,
                    "request handler panicked; answering 500 and continuing to serve"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"error":"internal server error"}"#,
                )
                    .into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panic_becomes_an_error_carrying_its_message() {
        let r = catch_panic(async {
            if true {
                panic!("poison input {}", 7);
            }
            1
        })
        .await;
        assert_eq!(r.unwrap_err(), "poison input 7");

        let r = catch_panic(async { std::panic::panic_any(42u8) }).await;
        assert!(r.unwrap_err().contains("non-string"));
    }

    #[tokio::test]
    async fn a_future_that_does_not_panic_passes_its_output_through() {
        assert_eq!(catch_panic(async { 5 }).await, Ok(5));
    }

    #[cfg(feature = "catch-panic")]
    mod http_tests {
        use super::super::http::catch_panic;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt;

        fn app() -> Router {
            Router::new()
                .route(
                    "/boom/{id}",
                    get(|| async {
                        if true {
                            panic!("secret internal detail");
                        }
                        "unreachable"
                    }),
                )
                .route("/ok", get(|| async { "fine" }))
                .layer(axum::middleware::from_fn(catch_panic))
        }

        async fn call(app: &Router, path: &str) -> (StatusCode, String) {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        }

        #[tokio::test]
        async fn a_panicking_handler_is_a_500_and_the_next_request_still_works() {
            let app = app();
            let (status, body) = call(&app, "/boom/1").await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(!body.contains("secret internal detail"), "{body}");

            // The same router (same service state) keeps answering, panics or not.
            assert_eq!(call(&app, "/ok").await, (StatusCode::OK, "fine".into()));
            assert_eq!(
                call(&app, "/boom/2").await.0,
                StatusCode::INTERNAL_SERVER_ERROR
            );
            assert_eq!(call(&app, "/ok").await, (StatusCode::OK, "fine".into()));
        }
    }
}
