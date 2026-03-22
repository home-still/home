# Provider and download source survey for academic paper acquisition

**Semantic Scholar, Europe PMC, and CrossRef are the highest-value additions to the existing arXiv + OpenAlex provider set.** Semantic Scholar provides 214M papers with direct OA PDF links and a clean JSON API. Europe PMC covers 33M biomedical papers with PDF URLs in search results and no authentication. CrossRef provides authoritative DOI metadata for 150M+ records but no PDFs — it pairs with Unpaywall (already integrated) for download resolution. CORE offers the largest OA full-text corpus (37M) but its free tier is severely rate-limited. Several other APIs (Dimensions, Lens.org, BASE) require paid subscriptions or manual approval and should be skipped.

---

## 1. Semantic Scholar — the clear next provider

Semantic Scholar (api.semanticscholar.org) indexes 214 million papers across all academic disciplines with particularly strong coverage in computer science and biomedical literature. The API is free with an optional API key that provides dedicated rate limits.

**Search:** `GET /graph/v1/paper/search?query={q}&fields={fields}&limit={n}&offset={n}` returns JSON. The `fields` parameter controls which data comes back — request `title,abstract,externalIds,openAccessPdf,year,authors,citationCount,influentialCitationCount` to get everything needed for display and download.

**DOI lookup:** `GET /graph/v1/paper/DOI:{doi}?fields={fields}` — clean, direct, no search parsing needed.

**PDF downloads:** The `openAccessPdf` field returns `{ url, status }` with a direct PDF link when available. The `status` field indicates `GREEN` (repository), `HYBRID` (publisher OA), or `BRONZE` (free-to-read). This is a high-quality signal — Semantic Scholar maintains its own OA detection separate from Unpaywall.

**Rate limits:** Without a key, requests share a pool of 1000 req/s across all unauthenticated users — effectively unreliable under load. With a free API key (requested at their site), the limit is 1 request per second per endpoint. The key is a simple Bearer token in the `x-api-key` header.

**Integration notes:** The response format maps cleanly to the existing `Paper` model. `externalIds` contains `{ DOI, ArXiv, PMID, PMCID, CorpusId }` which is valuable for cross-provider dedup — this is exactly the DOI equivalence map described in the metasearch UX research doc. The `influentialCitationCount` field (citations that meaningfully build on the work, per their citation intent classifier) is unique to Semantic Scholar and could differentiate our ranking from raw citation counts.

**Recommended config:**
```yaml
semantic_scholar:
  base_url: https://api.semanticscholar.org/graph/v1
  # api_key: your-key-here
  timeout_secs: 30
  rate_limit_interval_ms: 1100  # just over 1/s to stay safe
```

---

## 2. Europe PMC — best biomedical source

Europe PMC (europepmc.org) is a superset of PubMed with 33 million publications including PubMed, Agricola, European patents, and NHS guidelines. 6.5 million articles are open access with full text. The REST API requires no authentication.

**Search:** `GET /webservices/rest/search?query={query}&format=json&resultType=core&pageSize={n}&cursorMark={cursor}` — the `resultType=core` parameter is critical, returning full metadata including OA links. Without it, responses are sparse.

**DOI lookup:** `GET /webservices/rest/search?query=DOI:{doi}&format=json&resultType=core`

**PDF downloads:** The `fullTextUrlList` field in core results contains URLs from multiple sources (publisher, Europe PMC repository, Unpaywall). Each entry has `documentStyle` (pdf, html, doi) and `availability` (Open Access, Subscription) fields. Filter for `documentStyle: "pdf"` and `availability: "Open Access"` to get direct download URLs.

**Full text XML:** For papers in PMC, a dedicated endpoint returns structured XML: `GET /webservices/rest/{source}/{id}/fullTextXML`. This is useful for the distill pipeline but not for the CLI download flow.

**Rate limits:** Not explicitly documented. Europe PMC expects "reasonable use" — the current approach of per-provider rate limiting with a configurable interval handles this.

**Integration notes:** Europe PMC uses cursor-based pagination (`cursorMark`) rather than offset-based, which is more robust for large result sets but requires a different pagination model than arXiv/OpenAlex. The API returns MeSH terms and citation counts, which enrich paper metadata. This is the preferred biomedical API over PubMed E-utilities — simpler REST interface, JSON support, and direct OA PDF URLs that PubMed lacks.

**Recommended config:**
```yaml
europe_pmc:
  base_url: https://www.ebi.ac.uk/europepmc/webservices/rest
  timeout_secs: 30
  rate_limit_interval_ms: 200
```

---

## 3. CrossRef — authoritative DOI metadata

CrossRef (api.crossref.org) holds metadata for 150–180 million DOI records, making it the most comprehensive source for publication metadata across all disciplines. The API requires no authentication but offers a "polite pool" with better performance when a `mailto` parameter is included.

**Search:** `GET /works?query={q}&rows={n}&offset={n}&mailto={email}` — supports rich filters: `filter=from-pub-date:2020,type:journal-article,has-abstract:true`. The filter system is more expressive than any other provider.

**DOI lookup:** `GET /works/{doi}?mailto={email}` — definitive metadata for any CrossRef DOI.

**PDF downloads:** CrossRef does not provide OA PDF links. The `resource.primary.URL` field points to the publisher landing page. Use Unpaywall (already integrated) to resolve CrossRef DOIs to OA PDFs.

**Rate limits:** The polite pool (activated by including `mailto`) gets priority routing and better rate limits. Without it, requests may be throttled during peak load. No hard numbers are published.

**Integration notes:** CrossRef is best understood as a metadata authority, not a content source. Its value is in filling gaps that other providers miss: reliable publication dates, complete author lists, funder information, license data, and reference lists. The field preference rules from the metasearch UX doc place CrossRef highest for title/authors. CrossRef also provides citation counts via its `is-referenced-by-count` field, though OpenAlex's citation data is more comprehensive.

**Recommended config:**
```yaml
crossref:
  base_url: https://api.crossref.org
  mailto: you@example.com  # enables polite pool
  timeout_secs: 30
  rate_limit_interval_ms: 100
```

---

## 4. CORE — largest OA full-text corpus

CORE (core.ac.uk) aggregates 309 million metadata records and 37 million full texts from 14,000+ institutional repositories and data providers across 150+ countries. The v3 API is JSON-based with optional free API key.

**Search:** `GET /v3/search/works?q={query}&limit={n}&offset={n}` — supports boolean queries and field-specific search.

**DOI lookup:** `GET /v3/search/works?q=doi:"{doi}"`

**PDF downloads:** Results include `downloadUrl` for direct PDF download. A dedicated endpoint exists: `GET /v3/outputs/{id}/download`. CORE's strength is institutional repository content that often isn't available through Unpaywall — preprints, theses, technical reports, and grey literature.

**Rate limits:** This is the main constraint. Free unregistered access: 1 batch or 5 single requests per 10 seconds. Registered free access: slightly better but still significantly slower than other providers. Rate limit headers (`X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Retry-After`) make it possible to implement adaptive throttling.

**Integration notes:** CORE's slow rate limit makes it unsuitable as a primary search provider in a real-time fan-out. Better used as: (1) a fallback download resolver after Unpaywall fails, or (2) a batch enrichment source for papers already in the local collection. The circuit breaker pattern already in the resilience layer handles CORE's frequent slow responses well.

**Recommended config:**
```yaml
core:
  base_url: https://api.core.ac.uk/v3
  # api_key: your-key-here
  timeout_secs: 30
  rate_limit_interval_ms: 2100  # ~5 req/10s
```

---

## 5. DBLP — CS-specific discovery

DBLP (dblp.org) indexes millions of computer science publications with curated metadata going back to 1936. Conference proceedings coverage is particularly strong — DBLP is often the canonical reference for CS venue publications.

**Search:** `GET /search/publ/api?q={query}&format=json&h={hits}&f={offset}` — also supports author search (`/search/author/api`) and venue search (`/search/venue/api`).

**PDF downloads:** The `ee` (electronic edition) field contains URLs, but these are typically DOI links to publisher pages, not direct PDFs. Requires Unpaywall for download resolution.

**Integration notes:** DBLP's value is in CS-specific paper discovery with clean, curated metadata. Author disambiguation is excellent. However, it overlaps significantly with Semantic Scholar for CS content, and Semantic Scholar provides richer data (abstracts, citation counts, OA PDFs). Consider DBLP only if CS venue-level search (specific conferences/journals) is needed.

---

## 6. DOAJ — guaranteed open access

DOAJ (doaj.org) indexes 12.5 million articles from 22,699 open access journals. Everything in DOAJ is OA by definition — it's a curated directory, not a harvester.

**Search:** `GET /api/search/articles/{query}?page={n}&pageSize={n}` — supports field-specific queries like `bibjson.title:"{title}"` and `doi:{doi}`.

**PDF downloads:** Inconsistent. The `bibjson.link` array contains fulltext URLs, but many are HTML landing pages rather than direct PDFs. Since everything in DOAJ is OA, Unpaywall will successfully resolve most DOAJ DOIs to PDFs.

**Rate limits:** 2 requests per second with burst queue of 5.

**Integration notes:** DOAJ's primary value is as an OA signal — if a paper is in DOAJ, it's guaranteed freely available. Useful for filtering/faceting but not essential for discovery since OpenAlex already includes DOAJ data in its OA detection.

---

## 7. APIs to skip

**Dimensions** (app.dimensions.ai): API requires a paid institutional subscription. No free API tier. 130M+ papers but inaccessible for an open-source tool.

**Lens.org**: Free tier is web-only. Programmatic API requires a paid subscription plan. Integrates Unpaywall and OpenAlex data that we can access directly.

**BASE** (Bielefeld Academic Search Engine): 400M+ documents but API access requires manual IP approval by contacting Bielefeld University Library. Blocks automated requests. Not practical for distributed open-source use.

**Fatcat / Internet Archive Scholar**: Fatcat (api.fatcat.wiki) catalogs metadata with links to Internet Archive preserved PDFs, but the API is unreliable — timeouts are frequent. Internet Archive Scholar has no documented JSON API. Consider as a future preservation-focused fallback.

**OpenCitations**: Citation graph data only (billions of citation links). No paper search or download capability. Could enrich citation metadata but OpenAlex and Semantic Scholar already provide citation counts.

**PubMed E-utilities**: 36M biomedical citations, but XML-heavy API with no direct PDF URLs. Europe PMC is strictly superior — it includes all PubMed data plus additional European sources, with a simpler JSON REST API and OA PDF links in results.

---

## 8. Recommended download resolution chain

After finding a paper from any provider, resolve PDFs in this order:

1. **`paper.download_url`** — already populated by OpenAlex and Semantic Scholar during search. Zero additional network calls.
2. **Semantic Scholar `openAccessPdf`** — high-quality OA detection. If the paper was found via S2, the URL is already in hand.
3. **Unpaywall** (already integrated) — `best_oa_location.url_for_pdf`. Covers ~130M DOIs, ~50% hit rate for OA.
4. **Europe PMC `fullTextUrlList`** — biomedical papers with publisher and repository PDF links.
5. **CORE `downloadUrl`** — institutional repository content not in other sources. Rate-limited, use as last resort.

This chain maximizes PDF availability while minimizing API calls. Steps 1–2 are free (data from search results). Step 3 is one HTTP GET. Steps 4–5 are fallbacks for papers not found above.

---

## 9. Provider priority matrix

For the real-time fan-out search, providers should be assigned priorities based on response speed, coverage, and data quality:

| Provider | Priority | Typical response | Coverage | PDF URLs |
|---|---|---|---|---|
| OpenAlex | 90 (highest) | 200–400ms | 250M+, all disciplines | Some |
| Semantic Scholar | 85 | 300–600ms | 214M, all disciplines | Yes |
| Europe PMC | 75 | 400–800ms | 33M, biomedical | Yes |
| CrossRef | 70 | 300–500ms | 150M+, metadata only | No |
| CORE | 50 (lowest) | 1–3s | 309M metadata, 37M full text | Yes |
| DBLP | 60 | 200–400ms | Millions, CS only | No |

The existing RRF ranking with per-provider dedup handles heterogeneous result quality. Semantic Scholar's `influentialCitationCount` could be added as a ranking boost signal alongside raw citation counts.

---

## 10. Implementation order

1. **Semantic Scholar** — highest value, returns PDFs, follows `openalex.rs` pattern exactly
2. **CrossRef** — authoritative metadata, polite pool, enriches merge quality
3. **Europe PMC** — biomedical coverage, PDF URLs, cursor pagination (new pattern)
4. **CORE** — large OA corpus, primarily as download fallback due to rate limits
5. **DBLP** — CS-specific, only if users request venue-level search
