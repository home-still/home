# OpenAlex API: a production-ready deep dive for metasearch aggregators

OpenAlex is the most comprehensive open scholarly metadata API available today, indexing **240M+ works** (470M+ with the XPAC expansion) under a CC0 license. As of February 2026, the API underwent a major transformation: mandatory API keys, usage-based dollar pricing replacing flat rate limits, and new features including semantic search and full-text PDF downloads. For a metasearch aggregator, OpenAlex serves as the ideal backbone — but production use demands understanding its pricing model, data gaps (40% of records lack abstracts), and the nuances of its search, filter, and pagination systems. This report covers every detail needed to build reliably against this API.

---

## Authentication shifted to mandatory API keys and dollar-based pricing in February 2026

The OpenAlex rate-limiting system evolved through three distinct eras. The **legacy system** (pre-February 2026) offered 100,000 calls/day and 10 requests/second with an optional "polite pool" accessed by adding `mailto=you@example.com` to requests. No authentication was required. On **January 14, 2026**, Jason Priem announced mandatory API keys and a credit-based system effective February 13. By **February 24, 2026**, this evolved into the current **usage-based dollar pricing model**.

### Getting an API key

Create a free account at openalex.org (takes ~30 seconds), then copy your key from `openalex.org/settings/api`. Attach it to every request as `?api_key=YOUR_KEY`. Without a key, you get **$0.01/day** — roughly 100 list calls — suitable only for testing.

### Current pricing per request

| Operation | Cost/call | Cost/1,000 calls | Free daily budget ($1) |
|---|---|---|---|
| **Singleton** (lookup by ID/DOI) | **$0 (free)** | Free | Unlimited |
| **List + Filter** | $0.0001 | $0.10 | ~10,000 calls |
| **Search** (keyword) | $0.001 | $1.00 | ~1,000 calls |
| **Semantic search** | $0.01 | $10.00 | ~100 calls |
| **Content download** (PDF/XML) | $0.01 | $10.00 | ~100 calls |

Every API key receives **$1 of free usage per day**, resetting at midnight UTC. Singleton lookups are entirely free and unlimited — a critical optimization for aggregators that resolve DOIs in bulk. The hard rate ceiling is **100 requests/second** for all users regardless of plan.

### Monitoring and 429 behavior

Every response includes headers: `X-RateLimit-Limit` (daily budget in USD), `X-RateLimit-Remaining`, `X-RateLimit-Credits-Used` (cost of this request), and `X-RateLimit-Reset` (seconds until midnight UTC). The response body's `meta` object also includes `cost_usd`. A dedicated endpoint at `GET /rate-limit?api_key=YOUR_KEY` returns full budget status including per-endpoint costs. When limits are exceeded, the API returns **HTTP 429**. The recommended strategy is **exponential backoff** with retries.

### Paid tiers

Beyond the free $1/day, users can purchase **prepaid usage** via credit card at openalex.org/pricing. Institutional plans include **Member** ($5,000/year) with basic support, and **Member+** ($20,000/year) with data curation dashboards, consulting, pro API keys with higher quotas, and access to special filters like `from_updated_date` for hourly data sync. Academic researchers may qualify for free increased limits by contacting support@openalex.org.

---

## Search, filters, and pagination: three distinct query mechanisms

OpenAlex provides three search modes, a comprehensive filter system with 100+ fields for Works, and two pagination strategies with different tradeoffs.

### Three search modes

**Keyword search** (`?search=<query>`) searches titles, abstracts, and full text. It supports Boolean operators (`AND`, `OR`, `NOT`), parentheses for grouping, and double-quoted exact phrases. Stemming uses the Kstem token filter ("possums" matches "possum"), but only whole words match — "lun" will not find "lunar." **Wildcards (`*`, `?`, `~`) are stripped and not supported.** Results include a `relevance_score` property combining text similarity with a citation-count weighting, and are sorted by this score by default.

**Filter-based search** appends `.search` to a property name: `?filter=title.search:cubist` or `?filter=abstract.search:genomics`. The `.no_stem` variant disables stemming: `?filter=title.search.no_stem:surgery`. The convenience filter `title_and_abstract.search` searches both fields, while `fulltext.search` queries indexed full text (available for works with `has_fulltext:true`).

**Semantic search** (`?search.semantic=<query>`) is a newer feature using AI embeddings to match by meaning. A query about "machine learning in healthcare" finds papers using terms like "AI-driven medical diagnosis." This requires an API key and costs $0.01/call. You can paste entire abstracts as queries.

### Filter system syntax

Filters use the `?filter=attribute:value` syntax with these operators:

- **AND** (between different filters): comma-separated — `?filter=cited_by_count:>100,is_oa:true`
- **AND** (within same attribute): use `+` — `?filter=institutions.country_code:fr+gb` (works affiliated with both France AND UK)
- **OR** (within same filter): pipe `|` — `?filter=institutions.country_code:fr|gb` (France OR UK), **maximum 100 values**
- **NOT**: prefix with `!` — `?filter=type:!paratext`
- **Range**: `?filter=publication_year:2020-2024` or `?filter=cited_by_count:>100`
- **Cross-filter OR is not supported** — OR works only within a single filter attribute

### Key Works filter fields

The Works entity supports the most extensive filter set. Critical filters for aggregator use include: `doi`, `publication_year`, `from_publication_date`/`to_publication_date`, `type`, `language`, `is_oa`/`oa_status`, `cited_by_count`, `fwci`, `has_abstract`, `has_fulltext`, `has_doi`, `authorships.author.id`, `authorships.institutions.id`, `authorships.countries`, `primary_location.source.id`, `primary_topic.id`/`topics.id`, `keywords.keyword`, `grants.funder`, `indexed_in`, `is_retracted`, and `is_paratext`. The convenience filters `cites` and `cited_by` enable citation graph traversal: `?filter=cites:W2741809807` returns all works citing that paper. The `from_updated_date` filter (requires API key) enables incremental sync.

### Sorting options

Available sort properties: `display_name`, `cited_by_count`, `works_count`, `publication_date`, and `relevance_score` (only when a search filter is active). Direction defaults to ascending; append `:desc` for descending. Multi-sort is comma-separated: `?sort=publication_year:desc,relevance_score:desc`.

### Pagination: the 10,000-result wall

**Basic paging** uses `?page=N&per_page=M` (default 25, maximum 200). The hard ceiling is **page × per_page ≤ 10,000 results** — requesting beyond this returns an error. This suffices for user-facing search but fails for data harvesting.

**Cursor paging** breaks through this limit. Initialize with `?cursor=*`, then chain `meta.next_cursor` values from each response into subsequent requests. Continue until `next_cursor` is null. Cursor paging accesses unlimited results but is **sequential only** — no random page access. For aggregator use, cursor paging is essential for any filtered dataset exceeding 10,000 results.

The **`select` parameter** reduces payload size by requesting only specific root-level fields: `?select=id,doi,display_name,cited_by_count`. It cannot select nested fields (use `open_access`, not `open_access.is_oa`). Does not work with `group_by` or autocomplete.

### Group-by aggregations

The `?group_by=<property>` parameter returns faceted counts: `?group_by=oa_status` returns counts of works by OA status. Combinable with filters. Only one group_by per request. Maximum **200 groups per page**, with cursor paging available for more. Supports most filterable properties. Useful for building faceted search interfaces.

---

## The Works entity schema stores 240 million records with rich relational data

The Work object is the central entity and the most complex schema in OpenAlex. Understanding its structure — especially abstracts, citations, and the topic/concept distinction — is critical for aggregator design.

### Abstract storage: the inverted index format

Abstracts are stored as **inverted index objects**, not plain text, due to legal constraints inherited from Microsoft Academic Graph. Each key is a word, each value an array of integer positions:

```json
{"Despite": [0], "growing": [1], "interest": [2], "in": [3, 57, 73]}
```

Reconstruction requires creating an array of length max_position + 1, placing each word at its positions, and joining with spaces. The pyalex Python library handles this automatically. **Coverage stands at roughly 60% of recent works** but has been declining — Springer Nature requested abstract removal in November 2022, and Elsevier followed in November 2024, dropping overall coverage below 40% for some publication windows.

### Open access classification

The `open_access` object contains `is_oa` (boolean), `oa_status` (one of **gold**, **diamond**, **green**, **hybrid**, **bronze**, **closed**), `oa_url` (best available URL), and `any_repository_has_fulltext`. The `best_oa_location` object represents the highest-quality OA copy, scored by preferring publisher over repository, published version over accepted/submitted, and presence of a PDF URL. The `locations` array lists every known copy of the work, each with `source`, `version`, `license`, `is_oa`, `landing_page_url`, and `pdf_url`.

### Citation graph traversal

**Forward citations** (who cites this work): use `cited_by_count` for the count, `cited_by_api_url` for the pre-built query URL, or filter with `?filter=cites:W<id>`. **Backward citations** (what this work cites): the `referenced_works` field lists OpenAlex IDs of all cited works, with `referenced_works_count` for the total. **Year-by-year citation history**: `counts_by_year` provides the last 10 years. **Normalized impact**: `fwci` (Field-Weighted Citation Impact) and `citation_normalized_percentile` with `is_in_top_1_percent` and `is_in_top_10_percent` flags. **Related works**: algorithmically computed via concept overlap with recent papers. Note that **64% of records have zero referenced works**, so backward citation coverage has significant gaps.

### Topics vs. Concepts: the old and new classification systems

**Topics** (active, ~4,500) use a four-level hierarchy — Domain → Field → Subfield → Topic — developed with CWTS Leiden and based on citation clustering plus LLM labeling. Each work receives up to 3 topics with confidence scores. The model considers title, abstract, citations, and journal name.

**Concepts** (deprecated, ~65,000) used a six-level hierarchy mapped to Wikidata IDs, trained on the MAG corpus using titles, abstracts, and venue names. Score threshold was ≥0.3 for assignment. Concepts are still returned in API responses but are no longer maintained or updated. For new development, **use Topics exclusively**.

### Identifiers and external ID lookup

The `ids` object contains `openalex`, `doi` (always a full URL like `https://doi.org/...`), `pmid`, `pmcid`, and `mag`. Single-entity lookup supports multiple formats: `/works/W2741809807`, `/works/doi:10.7717/peerj.4375`, `/works/pmid:14907713`, `/works/pmcid:PMC12345`, `/works/mag:2741809807`. Merged entities return **HTTP 301** redirects to the canonical ID.

### Other entities at a glance

**Authors** include `display_name`, `orcid`, `last_known_institutions`, `affiliations` (with year ranges), `h_index`, `i10_index`, and `works_api_url`. Author disambiguation was overhauled in July 2023 with a new ML model; a complete rewrite using modern AI is planned for Q1 2026. **Sources** represent journals, repositories, and conferences with ISSN, publisher hierarchy, and DOAJ/CORE status. **Institutions** carry ROR IDs, geographic coordinates, parent-child relationships, and lineage arrays. **Publishers** have hierarchy levels and lineage. **Funders** (~32,000) link to works via **Awards** (formerly Grants). All entities have dehydrated versions (lightweight representations with only key fields) when embedded within other entity responses.

---

## Endpoint architecture follows a consistent REST pattern

The base URL is `https://api.openalex.org`. OpenAlex does **not use URL-based versioning** — the API evolves continuously with deprecation notices. The Walden rewrite (November 2025) used `?data-version=2`, which became the default on November 1, 2025.

### Core endpoints

All eight entity types follow the same pattern: `/<entities>` for lists, `/<entities>/<id>` for single lookup, `/<entities>/random` for a random entity. The **autocomplete endpoint** (`/autocomplete/<entity_type>?q=<query>`) returns results in ~200ms with `display_name`, `hint` (contextual — author's institution, institution's location, etc.), `cited_by_count`, and `external_id`. It supports concurrent filtering and is ideal for typeahead UIs.

**Batch lookups** use the OR pipe syntax in filters: `?filter=doi:https://doi.org/10.1234/a|https://doi.org/10.1234/b` fetches multiple works in one request (up to 100 IDs). Since singleton lookups are free, resolving individual DOIs is also cost-effective.

The **N-gram endpoint** (`/works/<id>/ngrams`) is **no longer in service** as a public endpoint, though n-grams still power full-text search internally. The **text endpoint** (`/text?title=<text>`) tags free text with OpenAlex topics and keywords but is marked for deprecation. **Content download** for ~60M open-access works is available via `content.openalex.org`.

The **sample parameter** (`?sample=50&seed=42`) returns reproducible random subsets for testing or statistical sampling, up to 10,000 per request.

---

## Building a production metasearch aggregator against OpenAlex

### Architectural strategy

Use OpenAlex as the **primary metadata backbone** and supplement with specialized sources. Crossref provides precise DOI metadata and publication dates. Semantic Scholar adds SPECTER embeddings, TL;DR summaries, and sometimes abstracts that OpenAlex lacks. PubMed is the gold standard for biomedical abstracts and MeSH terms. Unpaywall data is already embedded in OpenAlex's `open_access` fields (same parent organization, OurResearch). Join across sources using **normalized lowercase DOIs** as the universal key.

### Handling abstract gaps from major publishers

This is the most critical data quality challenge. Springer Nature and Elsevier both requested abstract removal from OpenAlex, and publishers like Taylor & Francis, IEEE, and ACS deposit zero abstracts to Crossref. The recommended fallback chain:

1. Check OpenAlex `abstract_inverted_index` first
2. Query Semantic Scholar by DOI for abstracts or TL;DR summaries
3. For biomedical works, fetch from PubMed E-utilities using PMID
4. Check Crossref for publisher-deposited abstracts
5. Flag records with missing abstracts rather than scraping publisher sites

### Fields to cache locally

For a production aggregator, index these fields from the Works entity: OpenAlex ID, DOI, title, `publication_year`, `publication_date`, `type`, `language`, the full `authorships` array (author ID, display_name, ORCID, institutions, position), `primary_location` (source ID, ISSN, publisher, type), `cited_by_count`, `fwci`, `referenced_works` list, `abstract_inverted_index`, `primary_topic` and `keywords`, `open_access` object (status, URL, best location), all external IDs (PMID, PMCID), `is_retracted`, `is_paratext`, and `updated_date` for sync tracking.

### Data freshness and bulk sync

The API ingests ~50,000 new works daily from Crossref. Public **snapshots** are released **quarterly** (moved from monthly in January 2025) as gzip-compressed JSON Lines on Amazon S3 — approximately **330 GB compressed, 1.6 TB decompressed**. Records are partitioned by `updated_date`, enabling incremental downloads of only changed data. Premium subscribers get monthly snapshots plus hourly API sync via `from_updated_date`. For initial load, always use the snapshot rather than paginating through the API.

### Client libraries

**pyalex** (Python, MIT license) is the most mature community library, supporting all entity types, automatic abstract reconstruction, cursor pagination, retry logic, and API key auth. Install via `pip install pyalex`. The **openalex** Rust crate (v0.2.2) exists but is minimally documented (0.21% coverage) and unsuitable for production without significant investment. The **papers-mcp** Rust crate provides an MCP server wrapping OpenAlex for AI assistants. For Rust production use, building a thin client with reqwest and serde against the REST API directly is the pragmatic path.

### Cost optimization for aggregator workloads

Singleton lookups are free — batch DOI resolution costs nothing. Use `per_page=200` (or 100, depending on endpoint) to maximize results per list/search call. Apply `select=` to reduce payload size. Prefer `filter` over `search` when possible ($0.0001 vs $0.001 per call). Use `group_by` for faceted counts instead of fetching all results. For datasets exceeding the free $1/day budget, estimate costs upfront: paginating through 694,000 Finnish-authored works costs ~$0.70 across ~7,000 list calls, well within the free tier. Paginating all 480M works via API would cost ~$480 — use the free snapshot instead.

---

## Known gotchas, data quality gaps, and upcoming changes

**Abstract coverage is declining**, not improving. Publisher takedown requests from Springer Nature (2022) and Elsevier (2024) have reduced coverage to roughly 60% for recent works. The January 2025 snapshot also removed 350,000 abstracts with invalid/junk content.

**Author disambiguation remains imperfect.** The July 2023 overhaul replaced all Author IDs (a rare stable-ID violation) and reduced single-work authors from 85M to 53M. A complete AI-based rewrite is promised for Q1 2026. The special ID A9999999999 represents the "Null Author" placeholder.

**Authorships are truncated to 100 entries** in list endpoint responses, with an `is_authors_truncated` flag. Works with >100 authors (~35,000 records) require individual entity fetches for complete author lists. Filtering by author ID also misses authors beyond position 100.

**The XPAC expansion (190M+ works)** from the Walden rewrite is excluded by default. These records — primarily datasets and repository items — have lower metadata quality. Include them explicitly with `?include_xpac=true` if coverage breadth matters more than metadata completeness.

**Document type classification** historically over-classified everything as "article." Improved in 2024 with more nuanced types, but >300,000 disagreements with Web of Science remain. **Language detection** uses `langdetect` on metadata text (not full text), causing widespread misidentification for non-English works with English-language titles and abstracts.

**Deprecation watchlist**: Concepts (use Topics), `host_venue`/`alternate_host_venues` (use `primary_location`/`locations`), `grants` (use `funders`/`awards`), the `/text` endpoint, and the old GitBook documentation at docs.openalex.org (being replaced by developers.openalex.org). The `from_created_date` and `from_updated_date` filters now require an API key (previously required Premium).

## Conclusion

OpenAlex occupies a unique position as the only truly open, comprehensive scholarly metadata API at scale. Its **free singleton lookups** and **$0.0001 list queries** make it remarkably cost-effective for aggregator architectures. The critical implementation decisions are: use the bulk snapshot for initial load (~330 GB), cursor pagination for filtered harvesting beyond 10,000 results, the `select` parameter aggressively to reduce payload, and a multi-source fallback chain (Semantic Scholar → PubMed → Crossref) to patch the growing abstract gap. The February 2026 pricing model actually benefits high-volume consumers — unlimited free DOI lookups alone eliminate the most common API cost in metasearch. The main risks are the declining abstract coverage from publisher takedowns and the still-imperfect author disambiguation, both of which the OpenAlex team has acknowledged and is actively addressing.