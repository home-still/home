//! Where does ollama run the embedding model?
//!
//! The embeddings behind the myelin store come from ollama's `bge-m3`, under
//! a model name whose batch is as large as the model's context. The project
//! rule is that distill embedding runs on the GPU and nowhere else, so the
//! server asks ollama where the model is resident — at startup, on `/health`
//! and `/readiness`, and before every index — and refuses to serve or index
//! when it is not fully on the GPU. At startup it also proves that the model
//! reads an input longer than ollama's stock batch to its end.
//!
//! Whole-card `nvidia-smi` numbers say nothing here (the card is shared); the
//! per-model `size` / `size_vram` pair from ollama's `/api/ps` is the signal.

use serde::Deserialize;

use crate::error::DistillError;
use crate::metadata::parse_ollama_url;

/// The embedding model myelin asks ollama for. Fixed: the vectors of one
/// collection must all come from one model.
///
/// It is `bge-m3` with `num_batch 8192`, made with `ollama create`. A stock
/// `bge-m3` has the same weights and a 2,048-token batch, and it embeds a
/// longer input from its first 2,048 tokens or refuses it (HTTP 413), so
/// that name is not accepted here: [`verify_embed_capacity`].
pub const OLLAMA_EMBED_MODEL: &str = "bge-m3-8k:latest";

/// How [`OLLAMA_EMBED_MODEL`] is made, for the error an ollama without it gets.
pub const OLLAMA_EMBED_MODEL_SETUP: &str = "`ollama pull bge-m3`, then `ollama create bge-m3-8k` \
     from a Modelfile holding `FROM bge-m3` and `PARAMETER num_batch 8192`";

/// Tokens ollama reads of one input with a model pulled as it is, whatever
/// the model's context says (measured, ollama 0.40.3).
const OLLAMA_STOCK_BATCH_TOKENS: u64 = 2048;

/// The word the capacity probe repeats: one token for bge-m3's tokenizer.
const CAPACITY_PROBE_WORD: &str = "the ";

/// Words in the capacity probe (measured: they come back as 2,102 tokens).
/// Just over the stock batch, which is the limit the probe exists to catch,
/// and no longer: ollama's runner keeps host memory for the longest input it
/// has read (measured 2026-11-26: 3.4 GiB after 2,102 tokens, 5.4 GiB after
/// 4,502, 9.0 GiB after 8,002).
const CAPACITY_PROBE_WORDS: u64 = 2100;

const _: () = assert!(CAPACITY_PROBE_WORDS > OLLAMA_STOCK_BATCH_TOKENS);

/// Device reported when the model is fully in VRAM.
pub const DEVICE_CUDA: &str = "cuda";

#[derive(Deserialize)]
struct Ps {
    #[serde(default)]
    models: Vec<Loaded>,
}

#[derive(Deserialize)]
struct Loaded {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    size_vram: u64,
}

fn backend(what: &str, e: impl std::fmt::Display) -> DistillError {
    DistillError::Backend(format!("ollama {what}: {e}"))
}

/// `"cuda"` when the whole model is in VRAM, `"cpu"` when none of it is,
/// `"partial"` when it is split between the card and system memory.
fn classify(size: u64, size_vram: u64) -> &'static str {
    if size_vram == 0 {
        "cpu"
    } else if size_vram >= size {
        DEVICE_CUDA
    } else {
        "partial"
    }
}

async fn loaded_model(
    http: &reqwest::Client,
    ps_url: &reqwest::Url,
) -> Result<Option<Loaded>, DistillError> {
    let response = http
        .get(ps_url.clone())
        .send()
        .await
        .map_err(|e| backend("unreachable", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(backend("/api/ps", format!("answered {status}")));
    }
    let ps: Ps = response
        .json()
        .await
        .map_err(|e| backend("/api/ps returned an unreadable body", e))?;
    Ok(ps.models.into_iter().find(|m| m.name == OLLAMA_EMBED_MODEL))
}

/// Where ollama has [`OLLAMA_EMBED_MODEL`] resident: `"cuda"`, `"cpu"` or
/// `"partial"`.
///
/// When the model is not loaded, one embedding request loads it and the
/// residency is read again; a model that is still absent after that is an
/// error, not a guess.
pub async fn ollama_embed_device(
    http: &reqwest::Client,
    ollama_url: &str,
) -> Result<String, DistillError> {
    let base = parse_ollama_url(ollama_url)?;
    let join = |path: &str| {
        base.join(path)
            .map_err(|e| DistillError::Config(format!("ollama_url {ollama_url:?}: {e}")))
    };
    let ps_url = join("api/ps")?;

    let mut model = loaded_model(http, &ps_url).await?;
    if model.is_none() {
        let response = http
            .post(join("api/embed")?)
            .json(&serde_json::json!({
                "model": OLLAMA_EMBED_MODEL,
                "input": "CUDA verification probe",
            }))
            .send()
            .await
            .map_err(|e| backend("embed probe failed", e))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(backend(
                "embed probe failed",
                format!(
                    "answered {status}: {}",
                    body.chars().take(200).collect::<String>()
                ),
            ));
        }
        model = loaded_model(http, &ps_url).await?;
    }
    let model = model.ok_or_else(|| {
        DistillError::Backend(format!("ollama did not load {OLLAMA_EMBED_MODEL}"))
    })?;
    Ok(classify(model.size, model.size_vram).to_string())
}

#[derive(Deserialize)]
struct Embedded {
    prompt_eval_count: Option<u64>,
}

/// Proves [`OLLAMA_EMBED_MODEL`] reads an input longer than ollama's stock
/// batch to its end.
///
/// One embedding request of [`CAPACITY_PROBE_WORDS`] one-token words; the
/// answer's `prompt_eval_count` is how many tokens the model read. A model
/// that read fewer would embed every long chunk from its beginning only, and
/// nothing downstream can tell, so the server refuses to start with it.
///
/// This shows the model is not a stock one. It does not measure the batch.
pub async fn verify_embed_capacity(
    http: &reqwest::Client,
    ollama_url: &str,
) -> Result<(), DistillError> {
    let url = parse_ollama_url(ollama_url)?
        .join("api/embed")
        .map_err(|e| DistillError::Config(format!("ollama_url {ollama_url:?}: {e}")))?;
    let probe = CAPACITY_PROBE_WORD.repeat(CAPACITY_PROBE_WORDS as usize);
    let response = http
        .post(url)
        .json(&serde_json::json!({"model": OLLAMA_EMBED_MODEL, "input": probe}))
        .send()
        .await
        .map_err(|e| backend("capacity probe failed", e))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(backend(
            "capacity probe failed",
            format!(
                "answered {status}: {}",
                body.chars().take(200).collect::<String>()
            ),
        ));
    }
    let embedded: Embedded = response
        .json()
        .await
        .map_err(|e| backend("capacity probe returned an unreadable body", e))?;
    let read = embedded.prompt_eval_count.ok_or_else(|| {
        backend(
            "capacity probe",
            "the answer does not say how many tokens were read",
        )
    })?;
    if read < CAPACITY_PROBE_WORDS {
        return Err(DistillError::Backend(format!(
            "ollama read {read} tokens of a {CAPACITY_PROBE_WORDS}-word probe with \
             {OLLAMA_EMBED_MODEL}: its batch is smaller than the model's context, so a long \
             chunk would be embedded from its beginning only. Make the model again: \
             {OLLAMA_EMBED_MODEL_SETUP}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{serve, Reply};

    fn ps(size: u64, size_vram: u64) -> String {
        serde_json::json!({"models": [
            {"name": "other:latest", "size": 1, "size_vram": 0},
            {"name": OLLAMA_EMBED_MODEL, "size": size, "size_vram": size_vram},
        ]})
        .to_string()
    }

    #[test]
    fn residency_is_classified_by_how_much_of_the_model_is_in_vram() {
        assert_eq!(classify(100, 100), "cuda");
        assert_eq!(classify(100, 120), "cuda");
        assert_eq!(classify(100, 0), "cpu");
        assert_eq!(classify(100, 60), "partial");
    }

    #[tokio::test]
    async fn a_resident_model_is_read_without_loading_anything() {
        let fake = serve(|_| Reply::Json(200, ps(1_200, 1_200))).await;
        let device = ollama_embed_device(&reqwest::Client::new(), &fake.url())
            .await
            .unwrap();
        assert_eq!(device, "cuda");
        let requests = fake.recorded();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].request_line.starts_with("GET /api/ps"));
    }

    #[tokio::test]
    async fn a_cpu_or_split_model_is_reported_as_such() {
        for (vram, want) in [(0, "cpu"), (600, "partial")] {
            let fake = serve(move |_| Reply::Json(200, ps(1_200, vram))).await;
            let device = ollama_embed_device(&reqwest::Client::new(), &fake.url())
                .await
                .unwrap();
            assert_eq!(device, want);
        }
    }

    #[tokio::test]
    async fn an_unloaded_model_is_loaded_by_one_embed_request_then_read_again() {
        let loaded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = loaded.clone();
        let fake = serve(move |req| {
            if req.request_line.starts_with("POST /api/embed") {
                assert!(req.body.contains(OLLAMA_EMBED_MODEL), "{}", req.body);
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Reply::Json(200, r#"{"embeddings":[[0.0]]}"#.into())
            } else if flag.load(std::sync::atomic::Ordering::SeqCst) {
                Reply::Json(200, ps(1_200, 1_200))
            } else {
                Reply::Json(200, r#"{"models":[]}"#.into())
            }
        })
        .await;
        let device = ollama_embed_device(&reqwest::Client::new(), &fake.url())
            .await
            .unwrap();
        assert_eq!(device, "cuda");
        let lines: Vec<String> = fake
            .recorded()
            .iter()
            .map(|r| r.request_line.clone())
            .collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[1].starts_with("POST /api/embed"), "{lines:?}");
    }

    #[tokio::test]
    async fn a_model_that_is_still_missing_after_the_probe_is_an_error() {
        let fake = serve(|req| {
            if req.request_line.starts_with("POST /api/embed") {
                Reply::Json(200, r#"{"embeddings":[[0.0]]}"#.into())
            } else {
                Reply::Json(200, r#"{"models":[]}"#.into())
            }
        })
        .await;
        let err = ollama_embed_device(&reqwest::Client::new(), &fake.url())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DistillError::Backend(m) if m.contains("did not load")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_ollama_error_status_is_a_backend_error() {
        let fake = serve(|_| Reply::Json(500, r#"{"error":"boom"}"#.into())).await;
        let err = ollama_embed_device(&reqwest::Client::new(), &fake.url())
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)), "{err}");
    }

    #[tokio::test]
    async fn an_invalid_url_is_a_config_error() {
        let err = ollama_embed_device(&reqwest::Client::new(), "not a url")
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Config(_)), "{err}");
    }

    fn embedded(tokens: u64) -> String {
        serde_json::json!({"embeddings": [[0.0]], "prompt_eval_count": tokens}).to_string()
    }

    #[tokio::test]
    async fn a_model_that_reads_the_whole_probe_passes() {
        let fake = serve(|_| Reply::Json(200, embedded(CAPACITY_PROBE_WORDS + 2))).await;
        verify_embed_capacity(&reqwest::Client::new(), &fake.url())
            .await
            .unwrap();
        let requests = fake.recorded();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].request_line.starts_with("POST /api/embed"));
        assert!(requests[0].body.contains(OLLAMA_EMBED_MODEL));
        assert_eq!(
            requests[0].body.matches(CAPACITY_PROBE_WORD).count() as u64,
            CAPACITY_PROBE_WORDS
        );
    }

    #[tokio::test]
    async fn a_model_that_reads_only_the_stock_batch_is_refused() {
        let fake = serve(|_| Reply::Json(200, embedded(OLLAMA_STOCK_BATCH_TOKENS))).await;
        let err = verify_embed_capacity(&reqwest::Client::new(), &fake.url())
            .await
            .unwrap_err();
        let read = format!("read {OLLAMA_STOCK_BATCH_TOKENS} tokens");
        assert!(
            matches!(&err, DistillError::Backend(m) if m.contains(&read) && m.contains("num_batch")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_probe_ollama_refuses_is_an_error() {
        let fake = serve(|_| Reply::Json(413, r#"{"error":"too large"}"#.into())).await;
        let err = verify_embed_capacity(&reqwest::Client::new(), &fake.url())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DistillError::Backend(m) if m.contains("413")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_answer_without_a_token_count_is_an_error() {
        let fake = serve(|_| Reply::Json(200, r#"{"embeddings":[[0.0]]}"#.into())).await;
        let err = verify_embed_capacity(&reqwest::Client::new(), &fake.url())
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)), "{err}");
    }
}
