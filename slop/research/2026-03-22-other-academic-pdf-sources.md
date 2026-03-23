# Every legitimate DOI-to-PDF resolver for your academic paper downloader

**Twenty-two programmatically accessible sources can resolve DOIs to PDF URLs or content**, forming a powerful resolver chain for an open-source CLI tool. The strongest strategy combines five broad aggregators (Unpaywall, OpenAlex, Fatcat, Semantic Scholar, CORE) with DOI-prefix routing to preprint servers and publisher-specific APIs, yielding OA PDF coverage for roughly 30–40% of all DOIs ever minted. Below is every viable source, organized into implementation tiers, with exact endpoints, auth requirements, rate limits, and legal status.

---

## Tier 1: Broad OA aggregators that should form your core chain

These five services cover the widest ground and should be queried in sequence for any DOI.

### 1. Unpaywall API

The gold standard for OA discovery. Purpose-built for exactly this use case.

**Endpoint:** `GET https://api.unpaywall.org/v2/{doi}?email={your_email}`

**Auth:** Email address as query parameter — no API key, no registration. Rate limit is per email. **100,000 requests/day.** The key response fields are `best_oa_location.url_for_pdf` (direct PDF URL, may be null), `best_oa_location.url` (PDF if available, else landing page), and `oa_locations[]` (all known copies, ranked). The `oa_status` field distinguishes gold, green, hybrid, bronze, and closed. Coverage spans **all ~150M+ Crossref DOIs**, finding OA copies for roughly **30–40%** of them across every discipline. All metadata is CC0. Run by OurResearch (nonprofit). Explicitly designed for tool integration. A convenience redirect at `https://unpaywall.org/{DOI}` sends users straight to the best OA copy.

**Gotcha:** `url_for_pdf` can be null even when `is_oa` is true — some locations only have landing pages. Always check multiple `oa_locations` entries.

### 2. OpenAlex API

Successor to Microsoft Academic Graph, built on Unpaywall data plus additional sources. **286M+ works indexed.** All data is CC0.

**Endpoint:** `GET https://api.openalex.org/works/https://doi.org/{doi}`

Note the DOI must be passed as a full URL. Auth requires a free API key (since February 2025), providing ~100,000 credits/day on the free tier. Key fields: `open_access.oa_url`, `best_oa_location.pdf_url`, and `locations[]` each with `pdf_url`, `version`, `license`, and `is_oa`. A batch filter endpoint accepts up to 100 pipe-separated DOIs: `?filter=doi:10.xxx/a|10.xxx/b`. OpenAlex is announcing a direct PDF content download endpoint (`/works/{id}/content/pdf`), making it increasingly the single best source. Since it ingests Unpaywall's data, OA coverage is essentially identical — but the richer metadata (authors, institutions, topics, funders) adds value.

### 3. Fatcat API (Internet Archive)

**The most underrated source in this list.** Fatcat catalogs research outputs and links them to archived PDF copies on the Wayback Machine.

**Endpoint:** `GET https://api.fatcat.wiki/v0/release/lookup?doi={doi}&expand=files&hide=abstracts,refs`

No authentication required for reads. The response includes a `files[]` array where each file has `mimetype` and `urls[]` with `rel` types: `"webarchive"` (Wayback Machine URLs — most reliable), `"web"` (live URLs), `"publisher"`, and `"repository"`. Filter for `mimetype: "application/pdf"`, then prefer URLs where `rel == "webarchive"` and the URL contains `//web.archive.org/`. Coverage draws from Crossref, PubMed, arXiv, DOAJ, Unpaywall, CORE, and web crawling — hundreds of millions of releases. Full OpenAPI spec at `https://api.fatcat.wiki/v0/openapi2.yml`. **No formal rate limit documentation**, but designed for moderate use; bulk consumers should use database dumps. All metadata is CC0, software is AGPL-3.0.

### 4. Semantic Scholar API

AI-powered academic graph from Allen Institute for AI. **~200M+ papers.**

**Endpoint:** `GET https://api.semanticscholar.org/graph/v1/paper/{doi}?fields=openAccessPdf,isOpenAccess,externalIds`

Pass the bare DOI as the paper ID. You **must** explicitly request `fields=openAccessPdf` — it's not returned by default. The response includes `openAccessPdf.url` (direct PDF link) and `openAccessPdf.status` (GREEN/GOLD/HYBRID/BRONZE). Without an API key: shared pool of 1,000 req/s across all anonymous users. With a free API key (header `x-api-key`): **1 request/second dedicated**. A batch endpoint at `POST /graph/v1/paper/batch` accepts up to 500 IDs per call. Free for non-commercial and research use with attribution. Strong in CS, AI, and biomedicine. PDF URL coverage is sparser than Unpaywall but occasionally finds copies Unpaywall misses.

### 5. CORE API (v3)

Aggregates full text from **10,000+ institutional repositories**. Uniquely, it indexes actual full-text content, not just metadata.

**Endpoint:** `GET https://api.core.ac.uk/v3/search/works` with query `doi:{doi}` (Elasticsearch syntax)

Requires a free API key (register at core.ac.uk). Rate limits: **5 requests per 10 seconds** on single endpoints. The response includes `downloadUrl` (direct PDF link when available) and sometimes `fullText` (the actual extracted text). Coverage: **200M+ metadata records**, with a significant portion having full text. Particularly strong for Green OA content deposited in institutional repositories. Commercial use requires a paid license; non-commercial is free. The main limitation: not all repository records include DOIs, so DOI-based lookup may miss papers that CORE actually has.

---

## Tier 2: The PubMed ecosystem for biomedical papers

For any DOI in the biomedical domain, these three services provide excellent coverage — and the PMC OA Web Service returns **direct FTP links to PDFs**.

### 6. PMC ID Converter + PMC OA Web Service

A two-step process that yields high-quality PDF links for the **~3.5M+ articles in PMC's Open Access subset**.

**Step 1 — Convert DOI to PMCID:**
`GET https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?ids={doi}&idtype=doi&format=json&tool=mytool&email=me@example.com`

Up to 200 IDs per request. Returns PMCID, PMID, and DOI mappings.

**Step 2 — Get PDF link:**
`GET https://www.ncbi.nlm.nih.gov/pmc/utils/oa/oa.fcgi?id={PMCID}`

Returns XML with **direct FTP links** to PDFs: `ftp://ftp.ncbi.nlm.nih.gov/pub/pmc/oa_pdf/...`. No API key needed, but include `tool` and `email` parameters. Rate limit: **3 requests/second** without NCBI API key, 10/sec with one (free registration). Public domain US government resource — fully legal for any use. Coverage is limited to PMC's OA subset, but this includes all NIH-funded author manuscripts and a large corpus of biomedical OA literature.

### 7. Europe PMC REST API

A superset of PubMed covering **33M+ publications** with **6.5M+ OA full-text articles**.

**Endpoint:** `GET https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:{doi}&resultType=core&format=json`

No authentication or API key required. The `core` result type returns a `fullTextUrlList` field containing URLs to PDFs, HTML full text, and DOI links. The `isOpenAccess` field indicates OA status. A single query returns PMCID, PMID, and DOI in one response — very efficient. Coverage extends beyond PubMed to include Agricola, patents, and preprints (including bioRxiv/medRxiv via the PPR identifier system). Full-text XML retrieval: `GET /rest/{source}/{id}/fullTextXML`. Excellent for any biomedical DOI.

---

## Tier 3: DOI-prefix routing to preprint servers and repositories

These sources are best queried when the DOI prefix matches their namespace. Implement a prefix-based router for maximum efficiency.

### 8. arXiv — prefix `10.48550/arXiv.`

ArXiv has assigned DOIs to all new articles since February 2022. **The PDF URL is deterministic**: extract the arXiv ID from the DOI and construct `https://arxiv.org/pdf/{arxiv_id}`. For example, DOI `10.48550/arXiv.2107.05580` → `https://arxiv.org/pdf/2107.05580`. No API call needed. For older arXiv papers with publisher DOIs (not arXiv DOIs), the arXiv search API at `https://export.arxiv.org/api/query?search_query=doi:{doi}` can find matches, though this endpoint has known bugs with DOIs containing special characters. Rate limit: **1 request per 3 seconds**. All arXiv content is freely accessible. Coverage: **2.4M+ papers** in physics, math, CS, biology, economics, and statistics since 1991.

### 9. bioRxiv/medRxiv — prefix `10.1101/`

Cold Spring Harbor Labs preprint servers with a clean public API.

**Endpoint:** `GET https://api.biorxiv.org/details/biorxiv/{doi}`  (or `medrxiv` for medRxiv)

No auth required. The response includes version metadata. **Construct the PDF URL**: `https://www.biorxiv.org/content/{doi}v{version}.full.pdf`. The `/pubs/` endpoint accepts both preprint DOIs and published-version DOIs to find the preprint. Coverage: bioRxiv has **300K+ preprints** (biology/life sciences, since 2013) and medRxiv has **70K+** (health sciences, since 2019). Paginated at 100 results per call. All preprints are freely accessible under CC licenses.

### 10. Zenodo — prefix `10.5281/zenodo.`

CERN's open research repository. Assigns DOIs to all deposits.

**Endpoint:** `GET https://zenodo.org/api/records/{record_id}`

Extract the record ID directly from the DOI (`10.5281/zenodo.{record_id}`). The response includes a `files` array with direct download URLs: `https://zenodo.org/records/{id}/files/{filename}?download=1`. No authentication needed for public records. All metadata is CC0; file licenses are chosen by depositors (mostly open). Coverage: **3M+ records** across all disciplines — datasets, papers, software, presentations. Since 2013.

### 11. figshare — prefix `10.6084/m9.figshare.`

**Endpoint:** `GET https://api.figshare.com/v2/articles/{article_id}` → then `GET /articles/{id}/files`

Each file has a `download_url` for direct download. No auth for public content. Recommended rate: **1 request/second**. Coverage: **5M+ items** across all disciplines. Operated by Digital Science (commercial), but public content API is free.

### 12. OSF and hosted preprint servers — prefix `10.31219/osf.io/` or `10.31235/osf.io/`

The OSF API provides unified access to ~30+ community preprint servers (PsyArXiv, SocArXiv, EcoEvoRxiv, EdArXiv, and more).

**Endpoint:** `GET https://api.osf.io/v2/preprints/?filter[doi]={doi}`

Requires 2–3 API hops to reach the PDF: preprint → `primary_file` relationship → file versions → download link. Rate limits: **100 requests/hour** without token, **10,000/day** with a free Bearer token. Apache 2.0 licensed. Coverage spans social sciences, psychology, education, ecology, and more — each hosted server has its own discipline focus.

### 13. HAL (French national archive)

Solr-based API serving **4.4M+ records** with ~1.6M full-text deposits. Strong French research coverage across all disciplines.

**Endpoint:** `GET https://api.archives-ouvertes.fr/search/?q=doiId_s:"{doi}"&wt=json&fl=fileMain_s,uri_s,label_s`

No auth required. The `fileMain_s` field contains the PDF URL. PDFs also available at `https://hal.science/{hal-id}/document`. No formal rate limits documented. OAI-PMH compliant. Metadata under Creative Commons.

### 14. ChemRxiv — prefix `10.26434/chemrxiv`

Chemistry preprints hosted on Cambridge Open Engage (migrated from Figshare in 2021).

**Endpoint:** `GET https://chemrxiv.org/engage/chemrxiv/public-api/v1/items` (search by DOI in query params)

No API key required. A Python client library exists (`pip install chemrxiv`) with `client.item_by_doi(doi)` and `paper.download_pdf()`. All content is OA under CC licenses. The ChemRxiv FAQ explicitly permits metadata mining and file downloading via API. Coverage: chemistry preprints, growing rapidly.

---

## Tier 4: Metadata-enriched sources with partial PDF links

These sources primarily provide metadata but include PDF or full-text URLs for a subset of their records.

### 15. Crossref API

The authoritative source for all **~150M+ Crossref-registered DOIs**. The `link` array in the response sometimes contains publisher PDF URLs.

**Endpoint:** `GET https://api.crossref.org/works/{doi}`

Use polite pool by adding `?mailto=you@example.com` or a `User-Agent` header with your email. Rate limit: ~50 requests/second in polite pool. The `link` field contains objects with `content-type` (look for `application/pdf`), `URL`, and `intended-application` (`text-mining` or `similarity-checking`). **Critical caveat:** these URLs point to publisher servers, and access often requires a subscription or TDM agreement — they are not guaranteed to be freely downloadable. Filter with `?filter=has-full-text:true` to find records with TDM links. Also check the `license` field for `applies-to: "tdm"` entries. All Crossref metadata is free and open.

### 16. DOAJ (Directory of Open Access Journals)

If an article is in DOAJ, it is **guaranteed open access**.

**Endpoint:** `GET https://doaj.org/api/v4/search/articles/doi:{doi}`

No API key needed for search. The response includes `bibjson.link[]` with `type: "fulltext"` entries pointing to publisher OA content. Coverage: **~9M+ articles** from **20,000+ curated OA journals**. Metadata is CC0. Uses Elasticsearch query syntax — encode spaces as `%20`, not `+`.

### 17. PLOS API (all open access)

All PLOS articles are CC BY with predictable PDF URLs.

**Endpoint:** `GET http://api.plos.org/search?q=id:"{doi}"`

**Direct PDF construction:** `https://journals.plos.org/plosone/article/file?id={doi}&type=printable`. JATS XML also available with `&type=manuscript`. No auth required. Coverage: **~300K+ articles** across 7 PLOS journals. Bulk download available at `allof.plos.org/allofplos.zip`.

### 18. Springer Nature Open Access API

Returns full-text JATS XML (not PDF) for **460K+ OA articles** from BMC and SpringerOpen journals.

**Endpoint:** `GET http://api.springernature.com/openaccess/json?q=doi:{doi}&api_key={KEY}`

Free API key required (register at dev.springernature.com). Rate limit: **100 requests/minute**. The response includes actual full-text content in XML, not just a URL. Non-commercial use per API terms. Springer Nature also has a Metadata API covering 14M+ documents, which returns links but not full text.

---

## Tier 5: Regional and niche sources

### 19. J-STAGE (Japanese journals)

**Endpoint:** `GET https://api.jstage.jst.go.jp/searchapi/do?service=3&doi={doi}`

No auth required. Returns XML with article metadata and links. **5.9M+ articles** from 4,300+ Japanese journals, ~82% peer-reviewed. Many are free/OA. Documentation is primarily in Japanese.

### 20. SciELO (Latin American journals)

ArticleMeta API at `http://articlemeta.scielo.org/api/v1/article/` and CitedBy API at `http://citedby.scielo.org/api/v1/pid/?q={doi}`. Coverage: **1,800+ journals**, primarily Latin American and Iberian. BSD-2 licensed. DOI-to-PDF requires mapping through SciELO PIDs.

### 21. Dissemin

**Endpoint:** `GET https://dissem.in/api/{doi}`

No auth. Returns OA classification (`OA`, `OK`, `UNK`, `CLOSED`), `pdf_url`, and `records[]` with locations across repositories, publishers, and social networks. Includes SHERPA/RoMEO policy data. Sources include Crossref, CORE, BASE, HAL, arXiv, and PubMed. Open source (AGPL-3.0). French academic project — active but update frequency declining.

### 22. DBLP (computer science metadata)

**Endpoint:** `GET https://dblp.org/search/publ/api?q={query}&format=json`

Returns `ee` (electronic edition) URLs linking to publisher pages or DOIs — **does not host PDFs**. All metadata is CC0. **7M+ CS publication records.** Useful for discovering DOIs to then resolve via other services.

---

## What doesn't work (and why to skip it)

Several commonly suggested sources are not viable for an open-source CLI tool:

- **SSRN:** No public API, downloads require CAPTCHA completion, ToS prohibit scraping
- **JSTOR:** Most APIs deprecated; subscription-based with no programmatic OA tier
- **Project MUSE:** No API; subscription-based
- **GetFTR:** Requires institutional registration as an "integrator" — designed for discovery services, not individual tools
- **Research4Life/HINARI:** No API; geo-restricted to eligible low-income countries; institution-only access
- **BASE:** API requires IP whitelisting — impractical for distributed CLI users
- **Lens.org:** Requires approval process; ToS may conflict with open-source distribution
- **Dimensions:** No PDF URLs in API response
- **OpenDOAR:** Directory of repositories, not a paper search engine
- **ORCID:** Stores researcher profiles, not paper content
- **OA.Works/Open Access Button:** Sunsetting services; recommends Unpaywall as replacement
- **Kopernio/EndNote Click, ReadCube, LazyScholar:** Proprietary browser extensions with no public APIs
- **Paperity:** No public API exists despite proposals
- **DOI content negotiation for PDFs:** DataCite retired `application/pdf` support in 2019; content negotiation returns metadata only, not article content

---

## The optimal resolver chain for your Rust CLI

Implement resolution as a cascading pipeline with early termination on first PDF hit. The DOI prefix check is nearly free and should come first.

**Phase 1 — DOI prefix routing (instant, no API call needed):**
Match the DOI prefix and construct direct PDF URLs where possible: `10.48550/arXiv.` → arXiv PDF URL, `10.1101/` → bioRxiv/medRxiv API, `10.5281/zenodo.` → Zenodo API, `10.6084/m9.figshare.` → figshare API, `10.26434/chemrxiv` → ChemRxiv API, `10.31219/osf.io/` or `10.31235/osf.io/` → OSF API, `10.1371/` → PLOS direct PDF URL.

**Phase 2 — Broad aggregator cascade (parallel or sequential):**
Query Unpaywall first (highest OA hit rate, simplest auth). If no `url_for_pdf`, try Fatcat (archived copies others miss). Then Semantic Scholar `openAccessPdf`. Then CORE if the user has configured an API key.

**Phase 3 — Domain-specific fallbacks:**
For biomedical DOIs, run PMC ID Converter → PMC OA Web Service. Query Europe PMC. Check DOAJ for guaranteed-OA journals.

**Phase 4 — Metadata-derived links:**
Query Crossref for TDM `link[]` entries with `content-type: application/pdf`. Follow DOI redirect to landing page and parse `<meta name="citation_pdf_url">` tag as a last resort.

For each PDF URL obtained, **verify with an HTTP HEAD request** — URLs can go stale. Check `Content-Type: application/pdf` and follow redirects. Cache results aggressively by DOI.

---

## Conclusion

The landscape of legitimate DOI-to-PDF resolution is broader and richer than most developers realize. **Fatcat is the most underappreciated source** — it provides Wayback Machine–archived PDFs with no authentication, and its coverage complements Unpaywall well. The DOI-prefix routing strategy is the single highest-impact optimization: it eliminates API calls entirely for arXiv, PLOS, and other predictable-URL publishers. OpenAlex is converging toward becoming the single unified API for this entire workflow, with CC0 licensing and an upcoming direct PDF download endpoint. For maximum coverage, the combination of Unpaywall + Fatcat + Semantic Scholar + prefix routing + PMC should resolve PDFs for the vast majority of OA-available papers without requiring any paid subscriptions or institutional affiliation.