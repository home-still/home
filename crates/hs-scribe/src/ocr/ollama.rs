use super::region::RegionType;
use anyhow::{Context, Result};
use ollama_rs::{
    generation::completion::request::GenerationRequest, generation::images::Image,
    models::ModelOptions, Ollama,
};
use reqwest::Url;

pub struct OllamaBackend {
    client: Ollama,
    model: String,
}

impl OllamaBackend {
    /// Construct a backend pointing at `url`. Returns `Err` if `url` is
    /// not a parseable URL or lacks a host component — no silent fallback
    /// to localhost (ONE PATH).
    pub fn new(url: &str, model: &str, request_timeout: std::time::Duration) -> Result<Self> {
        if std::env::var("OLLAMA_NUM_PARALLEL").is_err() {
            tracing::warn!(
                "OLLAMA_NUM_PARALLEL is not set. Set it to 2 when starting Ollama \
                 for parallel processing: OLLAMA_NUM_PARALLEL=2 ollama serve"
            );
        }
        let parsed = Url::parse(url).with_context(|| format!("invalid ollama URL: {url}"))?;
        let host_str = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("ollama URL has no host: {url}"))?;
        let host = format!("{}://{}", parsed.scheme(), host_str);
        let port = parsed
            .port()
            .ok_or_else(|| anyhow::anyhow!("ollama URL has no explicit port: {url}"))?;
        // `ollama-rs` has no timeout of its own, so it is handed a client
        // built through hs-common's builder: connect and whole-request
        // limits (the call is non-streaming, so the request limit is the
        // generation limit).
        let http = hs_common::http::client_builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(request_timeout)
            .build()
            .context("failed to build the Ollama HTTP client")?;
        Ok(Self {
            client: Ollama::new_with_client(host, port, http),
            model: model.to_string(),
        })
    }

    pub async fn recognize(&self, image_bytes: &[u8]) -> Result<String> {
        self.recognize_region(image_bytes, RegionType::FullPage)
            .await
    }

    pub async fn recognize_region(
        &self,
        image_bytes: &[u8],
        region_type: RegionType,
    ) -> Result<String> {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image_bytes);
        let image = Image::from_base64(&b64);

        // num_ctx=8192 is the minimum safe value: image tokens consume ~4672 for a
        // 200 DPI letter page, plus ~500 prompt tokens, plus output headroom.
        // num_ctx=4096 silently truncates the image and produces garbage.
        //
        // Penalty params: Ollama's Go VLM runner silently drops repeat_penalty,
        // frequency_penalty, and presence_penalty (ollama#14493 / #10767). We set
        // them anyway because (a) future Ollama fix lands them on the wire,
        // (b) symmetry with the OpenAI-compat backend serving vLLM where they
        // DO fire. 1.10 is the safe band (per the project's analysis doc); 1.3
        // substitutes visually similar tokens (0→O, l→1) on numeric/tabular
        // OCR. top_k=1 is defensive against samplers that mis-handle T=0;
        // at T=0 it's a no-op on engines that resolve argmax correctly.
        let options = ModelOptions::default()
            .temperature(0.0)
            .top_k(1)
            .repeat_penalty(1.10)
            .repeat_last_n(256)
            .num_predict(4096)
            .num_ctx(8192);

        let request = GenerationRequest::new(self.model.clone(), region_type.prompt().to_string())
            .images(vec![image])
            .options(options);

        let response = self.client.generate(request).await.map_err(|e| {
            anyhow::anyhow!(
                "Ollama VLM request failed (model={}, url={}): {e}",
                self.model,
                self.client.uri()
            )
        })?;

        Ok(response.response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    fn expect_err(res: Result<OllamaBackend>) -> anyhow::Error {
        match res {
            Ok(_) => panic!("expected OllamaBackend::new to error"),
            Err(e) => e,
        }
    }

    #[test]
    fn rejects_non_url_input() {
        let err = expect_err(OllamaBackend::new("not a url", "model", TIMEOUT));
        assert!(
            format!("{err:#}").contains("invalid ollama URL"),
            "error should mention invalid URL, got {err:#}"
        );
    }

    #[test]
    fn rejects_url_missing_port() {
        // Scheme+host parses, but there's no explicit port — the previous
        // fallback path quietly rewrote this to 11434 and sent traffic to
        // the local Ollama. Refuse loudly instead.
        let err = expect_err(OllamaBackend::new("http://remote-host/", "model", TIMEOUT));
        assert!(
            format!("{err:#}").contains("no explicit port"),
            "error should mention missing port, got {err:#}"
        );
    }

    #[test]
    fn accepts_well_formed_url() {
        let backend = OllamaBackend::new("http://127.0.0.1:11434", "gemma", TIMEOUT)
            .expect("well-formed URL should parse");
        assert_eq!(backend.model, "gemma");
    }

    #[tokio::test]
    async fn a_stalled_ollama_fails_the_call_at_the_timeout_instead_of_hanging() {
        // Accepts the connection and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let backend = OllamaBackend::new(
            &format!("http://127.0.0.1:{port}"),
            "m",
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let err = backend.recognize(b"jpeg").await.unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains("Ollama VLM request failed"),
            "{err:#}"
        );
    }
}
