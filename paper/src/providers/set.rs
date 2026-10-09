//! Every provider of one process, built once.
//!
//! Rate limits and circuit breakers only work if every request to a provider
//! passes through the *same* limiter and breaker. A short-lived CLI
//! invocation builds one [`ProviderSet`] and exits; a long-lived process
//! (`hs-mcp`) must build it **once at startup** and keep it, handing the
//! `Arc`s to each request:
//!
//! ```ignore
//! // at startup
//! let providers = Arc::new(ProviderSet::new(&Config::load()?)?);
//!
//! // per request — nothing is constructed here
//! let all = providers.provider(&ProviderArg::All);
//! let hit = all.get_by_doi(doi).await?;
//! let refs = providers.references(doi).await?;          // same S2 limiter/breaker
//! let downloader = PaperDownloader::with_event_bus(
//!     storage, events, &config.download, providers.download_resolvers())?;
//! ```
//!
//! Building a set per request (what `make_provider` allowed) gives every
//! request a fresh limiter and a fresh, closed breaker, i.e. no rate limiting
//! and no circuit breaking at all.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::cli::ProviderArg;
use crate::config::Config;
use crate::error::PaperError;
use crate::ports::provider::PaperProvider;
use crate::providers::arxiv::ArxivProvider;
use crate::providers::core::CoreProvider;
use crate::providers::crossref::CrossRefProvider;
use crate::providers::europe_pmc::EuropePmcProvider;
use crate::providers::openalex::OpenAlexProvider;
use crate::providers::resilient::ResilientProvider;
use crate::providers::semantic_scholar::{
    CitationsOpts, CitationsResponse, ReferencesResponse, SemanticScholarProvider,
};
use crate::resilience::config::ResilienceConfig;
use crate::resilience::guard::Guard;
use crate::services::search::AggregateProvider;

/// How long one provider may take inside an aggregate fan-out.
pub const AGGREGATE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ProviderSet {
    arxiv: Arc<dyn PaperProvider>,
    openalex: Arc<dyn PaperProvider>,
    semantic_scholar: Arc<dyn PaperProvider>,
    europe_pmc: Arc<dyn PaperProvider>,
    crossref: Arc<dyn PaperProvider>,
    core: Arc<dyn PaperProvider>,
    /// CORE is only a download resolver when an API key is configured.
    core_has_key: bool,
    /// Semantic Scholar's citation-graph endpoints: the raw provider plus the
    /// *same* guard the search/DOI wrapper above uses.
    s2_graph: Arc<SemanticScholarProvider>,
    s2_guard: Arc<Guard>,
    aggregate: Arc<AggregateProvider>,
}

impl ProviderSet {
    /// Build every provider, its rate limiter and its circuit breaker. Fails
    /// on the first provider that cannot be built (bad URL, zero interval …)
    /// rather than starting with a silently smaller set.
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        config.validate()?;
        let p = &config.providers;
        let r = &config.resilience;

        let arxiv = guarded(
            "arxiv",
            ArxivProvider::new(&p.arxiv).context("arXiv")?,
            p.arxiv.rate_limit_interval_ms,
            r,
        )?;
        let openalex = guarded(
            "openalex",
            OpenAlexProvider::new(&p.openalex).context("OpenAlex")?,
            p.openalex.rate_limit_interval_ms,
            r,
        )?;
        let europe_pmc = guarded(
            "europe_pmc",
            EuropePmcProvider::new(&p.europe_pmc).context("Europe PMC")?,
            p.europe_pmc.rate_limit_interval_ms,
            r,
        )?;
        let crossref = guarded(
            "crossref",
            CrossRefProvider::new(&p.crossref).context("CrossRef")?,
            p.crossref.rate_limit_interval_ms,
            r,
        )?;
        let core = guarded(
            "core",
            CoreProvider::new(&p.core).context("CORE")?,
            p.core.rate_limit_interval_ms,
            r,
        )?;

        let s2_graph = Arc::new(
            SemanticScholarProvider::new(&p.semantic_scholar).context("Semantic Scholar")?,
        );
        let s2_guard = Arc::new(Guard::new(
            "semantic_scholar",
            Duration::from_millis(p.semantic_scholar.rate_limit_interval_ms),
            r,
        )?);
        let semantic_scholar: Arc<dyn PaperProvider> =
            Arc::new(ResilientProvider::new(s2_graph.clone(), s2_guard.clone()));

        let aggregate = Arc::new(AggregateProvider::new(
            vec![
                arxiv.clone(),
                openalex.clone(),
                semantic_scholar.clone(),
                europe_pmc.clone(),
                crossref.clone(),
                core.clone(),
            ],
            AGGREGATE_TIMEOUT,
        ));

        Ok(Self {
            arxiv,
            openalex,
            semantic_scholar,
            europe_pmc,
            crossref,
            core,
            core_has_key: p.core.api_key.is_some(),
            s2_graph,
            s2_guard,
            aggregate,
        })
    }

    /// The shared instance behind `which`. Repeated calls return the same
    /// `Arc` (same limiter, same breaker).
    pub fn provider(&self, which: &ProviderArg) -> Arc<dyn PaperProvider> {
        match which {
            ProviderArg::Arxiv => self.arxiv.clone(),
            ProviderArg::OpenAlex => self.openalex.clone(),
            ProviderArg::SemanticScholar => self.semantic_scholar.clone(),
            ProviderArg::EuropePmc => self.europe_pmc.clone(),
            ProviderArg::CrossRef => self.crossref.clone(),
            ProviderArg::Core => self.core.clone(),
            ProviderArg::All => self.aggregate.clone(),
        }
    }

    /// A fresh aggregate over the *shared* members, for a caller that wants
    /// its own progress callback (`AggregateProvider::on_provider_done`).
    /// Limiter and breaker state stay shared; only the callback is private.
    pub fn aggregate(&self) -> AggregateProvider {
        AggregateProvider::new(self.members(), AGGREGATE_TIMEOUT)
    }

    /// Providers consulted, in order, to find a PDF URL for a DOI:
    /// Semantic Scholar, Europe PMC, CORE (only with an API key), OpenAlex,
    /// CrossRef. Pass to `PaperDownloader::with_event_bus`.
    pub fn download_resolvers(&self) -> Vec<Arc<dyn PaperProvider>> {
        let mut resolvers = vec![self.semantic_scholar.clone(), self.europe_pmc.clone()];
        if self.core_has_key {
            resolvers.push(self.core.clone());
        }
        resolvers.push(self.openalex.clone());
        resolvers.push(self.crossref.clone());
        resolvers
    }

    /// Semantic Scholar reference list, under the same guard as its search.
    pub async fn references(&self, doi: &str) -> Result<ReferencesResponse, PaperError> {
        self.s2_guard.run(|| self.s2_graph.references(doi)).await
    }

    /// Semantic Scholar citing papers. Each page request runs under the same
    /// guard as its search (limiter, breaker, retry), not the call as a whole.
    pub async fn citations(
        &self,
        doi: &str,
        opts: CitationsOpts,
    ) -> Result<CitationsResponse, PaperError> {
        self.s2_graph.citations(doi, opts, &self.s2_guard).await
    }

    fn members(&self) -> Vec<Arc<dyn PaperProvider>> {
        vec![
            self.arxiv.clone(),
            self.openalex.clone(),
            self.semantic_scholar.clone(),
            self.europe_pmc.clone(),
            self.crossref.clone(),
            self.core.clone(),
        ]
    }
}

fn guarded<P: PaperProvider + 'static>(
    name: &'static str,
    inner: P,
    rate_limit_ms: u64,
    resilience: &ResilienceConfig,
) -> anyhow::Result<Arc<dyn PaperProvider>> {
    let guard = Arc::new(Guard::new(
        name,
        Duration::from_millis(rate_limit_ms),
        resilience,
    )?);
    Ok(Arc::new(ResilientProvider::new(Arc::new(inner), guard)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn the_same_arc_comes_back_every_time() {
        let set = ProviderSet::new(&Config::default()).unwrap();
        for arg in [
            ProviderArg::Arxiv,
            ProviderArg::OpenAlex,
            ProviderArg::SemanticScholar,
            ProviderArg::EuropePmc,
            ProviderArg::CrossRef,
            ProviderArg::Core,
            ProviderArg::All,
        ] {
            assert!(
                Arc::ptr_eq(&set.provider(&arg), &set.provider(&arg)),
                "{arg:?} was rebuilt between calls"
            );
        }
    }

    #[test]
    fn a_fresh_aggregate_reuses_the_shared_members() {
        let set = ProviderSet::new(&Config::default()).unwrap();
        assert_eq!(set.aggregate().provider_count(), 6);
        // Same members, so same guards: the `Arc` strong count rises when an
        // aggregate holds them and falls when it is dropped.
        let before = Arc::strong_count(&set.arxiv);
        let agg = set.aggregate();
        assert_eq!(Arc::strong_count(&set.arxiv), before + 1);
        drop(agg);
        assert_eq!(Arc::strong_count(&set.arxiv), before);
    }

    #[test]
    fn core_is_a_download_resolver_only_with_an_api_key() {
        let mut config = Config::default();
        let names = |set: &ProviderSet| -> Vec<&'static str> {
            set.download_resolvers().iter().map(|p| p.name()).collect()
        };
        assert_eq!(
            names(&ProviderSet::new(&config).unwrap()),
            ["semantic_scholar", "europe_pmc", "openalex", "crossref"]
        );
        config.providers.core.api_key = Some("key".into());
        assert_eq!(
            names(&ProviderSet::new(&config).unwrap()),
            [
                "semantic_scholar",
                "europe_pmc",
                "core",
                "openalex",
                "crossref"
            ]
        );
    }

    #[test]
    fn an_invalid_config_is_an_error_not_a_smaller_set() {
        let mut config = Config::default();
        config.providers.crossref.rate_limit_interval_ms = 0;
        assert!(ProviderSet::new(&config).is_err());
    }

    #[tokio::test]
    async fn search_lookup_and_citation_graph_calls_share_one_breaker() {
        // A Semantic Scholar that is down. Two failures anywhere (one DOI
        // lookup, one reference fetch) open the breaker for *every* path to
        // it; the following calls never reach the server.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let mut config = Config::default();
        config.providers.semantic_scholar.base_url = server.uri();
        config.providers.semantic_scholar.rate_limit_interval_ms = 1;
        config.resilience.cb_failure_threshold = 2;
        config.resilience.retry_max_attempts = 0;
        let set = ProviderSet::new(&config).unwrap();
        let s2 = set.provider(&ProviderArg::SemanticScholar);

        assert!(matches!(
            s2.get_by_doi("10.1/a").await,
            Err(PaperError::ProviderUnavailable(_))
        ));
        assert!(matches!(
            set.references("10.1/a").await,
            Err(PaperError::ProviderUnavailable(_))
        ));

        assert!(matches!(
            s2.get_by_doi("10.1/a").await,
            Err(PaperError::CircuitBreakerOpen(_))
        ));
        assert!(matches!(
            set.references("10.1/a").await,
            Err(PaperError::CircuitBreakerOpen(_))
        ));
        assert!(matches!(
            set.citations("10.1/a", CitationsOpts::default()).await,
            Err(PaperError::CircuitBreakerOpen(_))
        ));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "open breaker must stop traffic to the provider"
        );
    }
}
