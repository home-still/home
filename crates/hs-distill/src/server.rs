//! axum adapter over [`crate::api`]: routing, JSON, status codes, NDJSON
//! streaming. No request logic lives here.
//!
//! RA-26 (deferred): `/collection/reset`, `DELETE /doc/{doc_id}` and
//! `/scrub-interstitials` are destructive and, like every other route, carry
//! no authentication; the server binds all interfaces. The bearer mechanism
//! for backend services is being designed separately — these three routes
//! are the ones that must adopt it first.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;

pub use crate::api::DistillServerState;
use crate::api::{ApiError, ErrorKind, IndexRequest, SearchRequest};
use crate::client::{DistillProgress, DistillStreamLine};

pub fn app(state: Arc<DistillServerState>) -> Router {
    Router::new()
        .route("/distill", post(handle_distill))
        .route("/distill/stream", post(handle_distill_stream))
        .route("/search", post(handle_search))
        .route("/health", get(handle_health))
        .route("/readiness", get(handle_readiness))
        .route("/status", get(handle_status))
        .route("/exists/{doc_id}", get(handle_exists))
        .route("/doc/{doc_id}", axum::routing::delete(handle_delete_doc))
        .route("/docs", get(handle_list_docs))
        .route("/collection/reset", post(handle_reset_collection))
        .route("/scrub-interstitials", post(handle_scrub_interstitials))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

fn error_response(e: ApiError) -> Response {
    let status = match e.kind {
        ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
        ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.message).into_response()
}

fn reply<T: serde::Serialize>(result: Result<T, ApiError>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(e),
    }
}

#[derive(Deserialize)]
struct CollectionQuery {
    collection: Option<String>,
}

#[derive(Deserialize)]
struct ScrubQuery {
    /// When true, scan and report only — do not delete. Default false.
    #[serde(default)]
    dry_run: bool,
    collection: Option<String>,
}

#[derive(Deserialize)]
struct ListDocsQuery {
    limit: Option<u64>,
    collection: Option<String>,
}

async fn handle_health(State(state): State<Arc<DistillServerState>>) -> Response {
    reply(state.health().await)
}

/// 200 with `ready: true`, or 503 with the same JSON body and a `reason`.
async fn handle_readiness(State(state): State<Arc<DistillServerState>>) -> Response {
    let readiness = state.readiness().await;
    let status = if readiness.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(readiness)).into_response()
}

async fn handle_status(
    State(state): State<Arc<DistillServerState>>,
    Query(q): Query<CollectionQuery>,
) -> Response {
    reply(state.status(q.collection.as_deref()).await)
}

async fn handle_delete_doc(
    State(state): State<Arc<DistillServerState>>,
    Path(doc_id): Path<String>,
    Query(q): Query<CollectionQuery>,
) -> Response {
    reply(
        state
            .delete_doc(&doc_id, q.collection.as_deref())
            .await
            .map(|deleted| serde_json::json!({"doc_id": doc_id, "deleted": deleted})),
    )
}

async fn handle_list_docs(
    State(state): State<Arc<DistillServerState>>,
    Query(q): Query<ListDocsQuery>,
) -> Response {
    reply(
        state
            .list_docs(q.limit, q.collection.as_deref())
            .await
            .map(|docs| serde_json::json!({"doc_ids": docs.doc_ids, "truncated": docs.truncated})),
    )
}

async fn handle_reset_collection(
    State(state): State<Arc<DistillServerState>>,
    Query(q): Query<CollectionQuery>,
) -> Response {
    reply(
        state
            .reset_collection(q.collection.as_deref())
            .await
            .map(|(collection, deleted)| {
                serde_json::json!({"collection": collection, "deleted_points": deleted})
            }),
    )
}

async fn handle_scrub_interstitials(
    State(state): State<Arc<DistillServerState>>,
    Query(q): Query<ScrubQuery>,
) -> Response {
    reply(
        state
            .scrub_interstitials(q.dry_run, q.collection.as_deref())
            .await,
    )
}

async fn handle_exists(
    State(state): State<Arc<DistillServerState>>,
    Path(doc_id): Path<String>,
    Query(q): Query<CollectionQuery>,
) -> Response {
    reply(
        state
            .doc_exists(&doc_id, q.collection.as_deref())
            .await
            .map(|(exists, chunks)| serde_json::json!({"exists": exists, "chunks": chunks})),
    )
}

async fn handle_distill(
    State(state): State<Arc<DistillServerState>>,
    Json(req): Json<IndexRequest>,
) -> Response {
    let job = match state.prepare_index(req) {
        Ok(job) => job,
        Err(e) => return error_response(e),
    };
    reply(state.run_index(job, |_| {}).await)
}

async fn handle_distill_stream(
    State(state): State<Arc<DistillServerState>>,
    Json(req): Json<IndexRequest>,
) -> Response {
    // A request that is wrong on its face is a 400 before any streaming
    // starts, not a 200 whose only line is an error.
    let job = match state.prepare_index(req) {
        Ok(job) => job,
        Err(e) => return error_response(e),
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);

    tokio::spawn(async move {
        let tx_progress = tx.clone();
        let on_progress = move |event: DistillProgress| {
            let line: DistillStreamLine = hs_common::service::protocol::StreamLine::Progress(event);
            if let Ok(json) = serde_json::to_string(&line) {
                let _ = tx_progress.try_send(Ok(format!("{json}\n")));
            }
        };

        let line: DistillStreamLine = match state.run_index(job, on_progress).await {
            Ok(result) => hs_common::service::protocol::StreamLine::Result(result),
            Err(e) => hs_common::service::protocol::StreamLine::Error(e.message),
        };
        if let Ok(json) = serde_json::to_string(&line) {
            let _ = tx.send(Ok(format!("{json}\n"))).await;
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, "text/x-ndjson")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn handle_search(
    State(state): State<Arc<DistillServerState>>,
    Json(req): Json<SearchRequest>,
) -> Response {
    reply(state.search(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{DistillStreamLine, ReadinessResponse};
    use crate::embed::EmbedderHealth;
    use crate::testutil::{prose, small_chunk_config, FakeEmbedder, FakeStore};
    use hs_common::service::protocol::StreamLine;

    /// A real axum server on an ephemeral loopback port, over fakes.
    struct Harness {
        base: String,
        http: reqwest::Client,
        embedder: Arc<FakeEmbedder>,
        store: Arc<FakeStore>,
    }

    async fn start() -> Harness {
        let embedder = Arc::new(FakeEmbedder::new());
        let store = Arc::new(FakeStore::default());
        let state = Arc::new(DistillServerState::new(
            embedder.clone(),
            store.clone(),
            small_chunk_config(),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });
        Harness {
            base: format!("http://{addr}"),
            http: reqwest::Client::new(),
            embedder,
            store,
        }
    }

    impl Harness {
        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }

        async fn post(&self, path: &str, body: serde_json::Value) -> reqwest::Response {
            self.http
                .post(self.url(path))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn distill_without_content_is_400_and_reads_nothing() {
        // RA-3 over the wire: the path names a readable file; it must not be
        // indexed, embedded, or searchable afterwards.
        let h = start().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hostname.md");
        std::fs::write(&file, "UNIQUE-SECRET-TEXT ".repeat(40)).unwrap();

        for route in ["/distill", "/distill/stream"] {
            for body in [
                serde_json::json!({ "path": file }),
                serde_json::json!({ "path": file, "content": null }),
            ] {
                let resp = h.post(route, body).await;
                assert_eq!(resp.status(), 400, "{route}");
                let text = resp.text().await.unwrap();
                assert!(text.contains("content"), "{route}: {text}");
                assert!(!text.contains("UNIQUE-SECRET"), "{route}: {text}");
            }
        }
        assert_eq!(h.embedder.calls(), 0);
        assert_eq!(h.store.ops(), []);
        assert!(h.store.state.lock().upserted.is_empty());
    }

    #[tokio::test]
    async fn distill_with_content_indexes_it() {
        let h = start().await;
        let resp = h
            .post(
                "/distill",
                serde_json::json!({ "path": "markdown/ab/doc.md", "content": prose(6) }),
            )
            .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["doc_id"], "doc");
        assert!(body["chunks_indexed"].as_u64().unwrap() > 0);
        assert!(!h.store.stored("academic_papers", "doc").is_empty());
    }

    #[tokio::test]
    async fn distill_stream_emits_progress_then_a_result_line() {
        let h = start().await;
        let text = h
            .post(
                "/distill/stream",
                serde_json::json!({ "path": "doc.md", "content": prose(6) }),
            )
            .await
            .text()
            .await
            .unwrap();
        let lines: Vec<DistillStreamLine> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(lines.len() >= 2, "{text}");
        assert!(matches!(lines[0], StreamLine::Progress(_)));
        match lines.last().unwrap() {
            StreamLine::Result(r) => assert_eq!(r.doc_id, "doc"),
            other => panic!("last line should be the result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failure_after_streaming_started_is_an_error_line() {
        let h = start().await;
        h.store.state.lock().fail_upsert_from_call = Some(0);
        let text = h
            .post(
                "/distill/stream",
                serde_json::json!({ "path": "doc.md", "content": prose(6) }),
            )
            .await
            .text()
            .await
            .unwrap();
        let last: DistillStreamLine = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert!(matches!(last, StreamLine::Error(_)), "{text}");
    }

    #[tokio::test]
    async fn unknown_collection_is_400_on_every_route_and_creates_nothing() {
        let h = start().await;
        let q = "collection=made_up";
        let responses = vec![
            h.post(
                "/distill",
                serde_json::json!({"path":"a.md","content":"x","collection":"made_up"}),
            )
            .await,
            h.post(
                "/distill/stream",
                serde_json::json!({"path":"a.md","content":"x","collection":"made_up"}),
            )
            .await,
            h.post(
                "/search",
                serde_json::json!({"query":"q","collection":"made_up"}),
            )
            .await,
            h.http
                .get(h.url(&format!("/status?{q}")))
                .send()
                .await
                .unwrap(),
            h.http
                .get(h.url(&format!("/exists/d?{q}")))
                .send()
                .await
                .unwrap(),
            h.http
                .get(h.url(&format!("/docs?{q}")))
                .send()
                .await
                .unwrap(),
            h.http
                .delete(h.url(&format!("/doc/d?{q}")))
                .send()
                .await
                .unwrap(),
            h.http
                .post(h.url(&format!("/collection/reset?{q}")))
                .send()
                .await
                .unwrap(),
            h.http
                .post(h.url(&format!("/scrub-interstitials?{q}")))
                .send()
                .await
                .unwrap(),
        ];
        for resp in responses {
            let url = resp.url().path().to_string();
            assert_eq!(resp.status(), 400, "{url}");
        }
        assert_eq!(h.store.ops(), []);
        assert_eq!(h.embedder.calls(), 0);
    }

    #[tokio::test]
    async fn configured_extra_collection_is_served() {
        let h = start().await;
        let resp = h
            .post(
                "/distill",
                serde_json::json!({"path":"a.md","content":prose(4),"collection":"personal_docs"}),
            )
            .await;
        assert_eq!(resp.status(), 200);
        assert!(!h.store.stored("personal_docs", "a").is_empty());
        assert!(h.store.stored("academic_papers", "a").is_empty());
    }

    #[tokio::test]
    async fn bad_input_is_4xx_and_dependency_failures_are_5xx() {
        let h = start().await;
        // 400s
        assert_eq!(
            h.post("/search", serde_json::json!({"query":"  "}))
                .await
                .status(),
            400
        );
        assert_eq!(
            h.post(
                "/search",
                serde_json::json!({"query":"q","filters":{"year":"abc"}})
            )
            .await
            .status(),
            400
        );
        assert_eq!(
            h.post("/distill", serde_json::json!({"path":"..","content":"x"}))
                .await
                .status(),
            400
        );
        assert_eq!(
            h.http
                .get(h.url("/docs?limit=99999999"))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        // 500: the store fails while indexing
        h.store.state.lock().fail_upsert_from_call = Some(0);
        assert_eq!(
            h.post(
                "/distill",
                serde_json::json!({"path":"a.md","content":prose(6)})
            )
            .await
            .status(),
            500
        );
    }

    #[tokio::test]
    async fn docs_lists_ids_with_a_truncation_flag() {
        let h = start().await;
        for d in ["a", "b", "c"] {
            h.store.seed("academic_papers", d, 1);
        }
        let all: serde_json::Value = h
            .http
            .get(h.url("/docs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(all["doc_ids"].as_array().unwrap().len(), 3);
        assert_eq!(all["truncated"], false);
        let some: serde_json::Value = h
            .http
            .get(h.url("/docs?limit=2"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(some["doc_ids"].as_array().unwrap().len(), 2);
        assert_eq!(some["truncated"], true);
    }

    #[tokio::test]
    async fn readiness_and_health_follow_the_embedder_and_qdrant() {
        let h = start().await;
        let ready = h.http.get(h.url("/readiness")).send().await.unwrap();
        assert_eq!(ready.status(), 200);
        let body: ReadinessResponse = ready.json().await.unwrap();
        assert!(body.ready);
        assert_eq!(body.capacity, 2);
        assert_eq!(
            h.http.get(h.url("/health")).send().await.unwrap().status(),
            200
        );

        *h.embedder.health.lock() = EmbedderHealth::Failed("slot 0 poisoned".into());
        let resp = h.http.get(h.url("/readiness")).send().await.unwrap();
        assert_eq!(resp.status(), 503);
        let body: ReadinessResponse = resp.json().await.unwrap();
        assert!(!body.ready);
        assert!(body.reason.unwrap().contains("poisoned"));
        assert_eq!(
            h.http.get(h.url("/health")).send().await.unwrap().status(),
            503
        );

        *h.embedder.health.lock() = EmbedderHealth::Healthy;
        h.store.state.lock().down = true;
        assert_eq!(
            h.http
                .get(h.url("/readiness"))
                .send()
                .await
                .unwrap()
                .status(),
            503
        );
        assert_eq!(
            h.http.get(h.url("/health")).send().await.unwrap().status(),
            503
        );
    }

    #[tokio::test]
    async fn delete_and_exists_round_trip() {
        let h = start().await;
        h.store.seed("academic_papers", "d", 3);
        let exists: serde_json::Value = h
            .http
            .get(h.url("/exists/d"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            (exists["exists"].clone(), exists["chunks"].clone()),
            (true.into(), 3.into())
        );
        let deleted: serde_json::Value = h
            .http
            .delete(h.url("/doc/d"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(deleted["deleted"], 3);
        let exists: serde_json::Value = h
            .http
            .get(h.url("/exists/d"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(exists["exists"], false);
    }
}
