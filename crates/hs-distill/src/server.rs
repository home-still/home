//! axum adapter over [`crate::api`]: routing, JSON, status codes, NDJSON
//! streaming. No request logic lives here.
//!
//! Every route except `GET /health` and `GET /readiness` requires the shared
//! backend bearer token (`hs_common::auth::backend`, enforced by the
//! middleware in [`router`]); the server binds all interfaces. The two open
//! probes report only whether a dependency is up, never its address or error
//! text — that detail is on the protected `/status`.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hs_common::auth::backend::BackendToken;
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;

pub use crate::api::DistillServerState;
use crate::api::{ApiError, ErrorKind, IndexRequest, SearchRequest};
use crate::client::{DistillProgress, DistillStreamLine};

/// The router. `GET /health` and `GET /readiness` are open (probes and
/// pools); every other route requires `Authorization: Bearer <token>`.
pub fn app(state: Arc<DistillServerState>, token: BackendToken) -> Router {
    let protected = Router::new()
        .route("/distill", post(handle_distill))
        .route("/distill/stream", post(handle_distill_stream))
        .route("/search", post(handle_search))
        .route("/status", get(handle_status))
        .route("/exists/{doc_id}", get(handle_exists))
        .route("/doc/{doc_id}", axum::routing::delete(handle_delete_doc))
        .route("/docs", get(handle_list_docs))
        .route("/collection/reset", post(handle_reset_collection))
        .route("/collection/hnsw", post(handle_enable_hnsw))
        .route("/scrub-interstitials", post(handle_scrub_interstitials))
        .route_layer(middleware::from_fn_with_state(token, require_token));
    Router::new()
        .route("/health", get(handle_health))
        .route("/readiness", get(handle_readiness))
        .merge(protected)
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
        // Outermost: a handler panic is a 500 and the server keeps serving.
        .layer(axum::middleware::from_fn(
            hs_common::panic_guard::http::catch_panic,
        ))
}

/// The backend secret the server requires, from `lookup` (the process
/// environment in production). Unset or unusable is an error naming
/// `HS_BACKEND_TOKEN` (never its value): the server refuses to start.
pub fn backend_token(
    lookup: impl Fn(&str) -> Result<String, std::env::VarError>,
) -> anyhow::Result<BackendToken> {
    BackendToken::from_lookup(lookup).map_err(|e| {
        anyhow::anyhow!(
            "hs-distill-server requires a backend token: {e:#}. Put HS_BACKEND_TOKEN \
             (>= 32 visible ASCII bytes, e.g. `openssl rand -hex 32`, the same value as the \
             gateway and clients) in ~/.home-still/secrets.env"
        )
    })
}

async fn require_token(
    State(token): State<BackendToken>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    match token.check_authorization(request.headers()) {
        Ok(()) => next.run(request).await,
        Err(why) => (
            StatusCode::UNAUTHORIZED,
            [
                (header::WWW_AUTHENTICATE, "Bearer realm=\"hs-distill\""),
                (header::CONTENT_TYPE, "application/json"),
            ],
            serde_json::json!({ "error": format!("unauthorized: {why}") }).to_string(),
        )
            .into_response(),
    }
}

async fn handle_enable_hnsw(
    State(state): State<Arc<DistillServerState>>,
    Query(q): Query<CollectionQuery>,
) -> Response {
    reply(state.enable_hnsw(q.collection.as_deref()).await)
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
    const TOKEN: &str = "test-backend-token-0123456789abcdef";

    fn token() -> BackendToken {
        BackendToken::new(TOKEN).unwrap()
    }

    /// Loopback client with explicit connect/total timeouts; `bearer` sets
    /// the Authorization header on every request.
    fn http_client(bearer: Option<&str>) -> reqwest::Client {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(b) = bearer {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {b}").parse().unwrap(),
            );
        }
        hs_common::http::client_builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .default_headers(headers)
            .build()
            .unwrap()
    }

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
            axum::serve(listener, app(state, token())).await.unwrap();
        });
        Harness {
            base: format!("http://{addr}"),
            http: http_client(Some(TOKEN)),
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
        let reason = body.reason.unwrap();
        assert!(reason.contains("embedder unusable"));
        assert!(
            !reason.contains("poisoned"),
            "internal detail leaked: {reason}"
        );
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

    #[tokio::test]
    async fn the_real_client_talks_to_the_real_router() {
        // Wire compatibility: every DistillClient call against the axum app.
        use crate::client::{DistillClient, SearchFilters};
        use hs_common::service::protocol::ReadinessInfo;

        let h = start().await;
        let client = DistillClient::new_with_client(
            &h.base,
            hs_common::auth::client::AuthedHttp::plain_with_backend_token(
                http_client(None),
                Some(token()),
            ),
        );

        let indexed = client
            .index_content("markdown/ab/doc.md", &prose(8), None)
            .await
            .unwrap();
        assert_eq!(indexed.doc_id, "doc");
        assert!(indexed.chunks_indexed > 0);
        assert_eq!(
            client.doc_chunks("doc").await.unwrap(),
            (true, u64::from(indexed.chunks_indexed))
        );
        assert_eq!(client.list_docs(10).await.unwrap(), ["doc"]);

        let status = client.status().await.unwrap();
        assert_eq!(status.documents_count, 1);
        assert_eq!(status.points_count, u64::from(indexed.chunks_indexed));
        assert!(!status.documents_count_truncated);

        let ready = client.readiness().await.unwrap();
        assert!(ready.is_ready());
        assert_eq!(ready.available_slots(), 2);
        assert_eq!(client.health().await.unwrap().embed_model, "bge-m3");

        client
            .search(
                "query",
                5,
                SearchFilters {
                    year: Some(">2000".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let err = client
            .search(
                "query",
                5,
                SearchFilters {
                    year: Some("nonsense".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("400"), "{err:#}");

        let unknown = client
            .index_content_in("a.md", "text", None, Some("made_up"))
            .await
            .unwrap_err();
        assert!(
            format!("{unknown:#}").contains("unknown collection"),
            "{unknown:#}"
        );

        assert_eq!(
            client.delete_doc("doc").await.unwrap(),
            u64::from(indexed.chunks_indexed)
        );
        assert_eq!(client.reset_collection().await.unwrap(), 0);
        let report = client.scrub_interstitials(true).await.unwrap();
        assert_eq!(report.matched, 0);
    }

    // ── RA-26: backend bearer token ────────────────────────────────────

    /// (method, path) of every protected route.
    fn protected_routes() -> Vec<(&'static str, &'static str)> {
        vec![
            ("POST", "/distill"),
            ("POST", "/distill/stream"),
            ("POST", "/search"),
            ("GET", "/status"),
            ("GET", "/exists/d"),
            ("DELETE", "/doc/d"),
            ("GET", "/docs"),
            ("POST", "/collection/reset"),
            ("POST", "/collection/hnsw"),
            ("POST", "/scrub-interstitials"),
        ]
    }

    async fn send(c: &reqwest::Client, method: &str, url: String) -> reqwest::Response {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        c.request(m, url)
            .json(&serde_json::json!({"path":"a.md","content":prose(4),"query":"q"}))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn every_route_but_the_probes_needs_the_token() {
        let h = start().await;
        for bearer in [None, Some("wrong-token-wrong-token-wrong-token-xx")] {
            let c = http_client(bearer);
            for (method, path) in protected_routes() {
                let resp = send(&c, method, h.url(path)).await;
                assert_eq!(resp.status(), 401, "{method} {path} bearer={bearer:?}");
                assert!(resp.headers().contains_key(header::WWW_AUTHENTICATE));
                let body: serde_json::Value = resp.json().await.unwrap();
                let msg = body["error"].as_str().unwrap();
                assert!(msg.starts_with("unauthorized"), "{msg}");
                assert!(!msg.contains(TOKEN));
            }
        }
        assert_eq!(h.store.ops(), [], "nothing may run unauthenticated");
        assert_eq!(h.embedder.calls(), 0);

        let open = http_client(None);
        for path in ["/health", "/readiness"] {
            let r = open.get(h.url(path)).send().await.unwrap();
            assert_eq!(r.status(), 200, "{path} must stay open");
        }
        // With the token every protected route gets past auth.
        for (method, path) in protected_routes() {
            let r = send(&h.http, method, h.url(path)).await;
            assert_ne!(r.status(), 401, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn enable_hnsw_submits_once_then_reports_a_no_op() {
        let h = start().await;
        let first: serde_json::Value = h
            .http
            .post(h.url("/collection/hnsw?collection=paper_abstracts"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(first["submitted"], true);
        assert_eq!(first["collection"], "paper_abstracts");
        assert_eq!(first["max_indexing_threads"], 4);
        let again: serde_json::Value = h
            .http
            .post(h.url("/collection/hnsw?collection=paper_abstracts"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(again["submitted"], false);
        assert_eq!(
            h.store.ops(),
            [crate::testutil::Op::EnableHnsw {
                collection: "paper_abstracts".into(),
                threads: 4
            }]
        );
        let unknown = h
            .http
            .post(h.url("/collection/hnsw?collection=made_up"))
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), 400);
    }

    #[tokio::test]
    async fn the_client_enables_hnsw_with_the_token() {
        let h = start().await;
        let client = crate::client::DistillClient::new_with_client(
            &h.base,
            hs_common::auth::client::AuthedHttp::plain_with_backend_token(
                http_client(None),
                Some(token()),
            ),
        );
        let r = client.enable_hnsw(None).await.unwrap();
        assert!(r.submitted);
        assert_eq!(r.collection, "academic_papers");

        let anon = crate::client::DistillClient::new_with_client(
            &h.base,
            hs_common::auth::client::AuthedHttp::plain_with_backend_token(http_client(None), None),
        );
        let err = anon.enable_hnsw(None).await.unwrap_err();
        assert!(format!("{err:#}").contains("401"), "{err:#}");
    }

    #[test]
    fn startup_refuses_a_missing_or_short_token_without_echoing_it() {
        let unset = backend_token(|_| Err(std::env::VarError::NotPresent)).unwrap_err();
        assert!(
            format!("{unset:#}").contains("HS_BACKEND_TOKEN"),
            "{unset:#}"
        );
        let short = backend_token(|_| Ok("sekrit-short".into())).unwrap_err();
        let msg = format!("{short:#}");
        assert!(
            msg.contains("HS_BACKEND_TOKEN") && !msg.contains("sekrit-short"),
            "{msg}"
        );
        backend_token(|_| Ok(TOKEN.into())).unwrap();
    }
}
