//! DuckDB schema for the OpenAlex catalog.
//!
//! All IDs are bare (no `https://openalex.org/` prefix) — see `parser::strip_*`.
//! Variable nested fields land in `JSON` columns (DuckDB's first-class type)
//! so we don't lock ourselves into one shape if OpenAlex evolves.
//!
//! Indexes are deliberately minimal at table-create time. Bulk indexes (the
//! FTS index, work_references composite) are built AFTER load via dedicated
//! steps in `duckdb_loader` so they don't slow ingest.

pub const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS concepts (
    openalex_id     VARCHAR PRIMARY KEY,
    display_name    VARCHAR,
    level           UTINYINT,
    description     VARCHAR,
    wikidata        VARCHAR,
    works_count     UBIGINT,
    cited_by_count  UBIGINT,
    ancestors       JSON
);

CREATE TABLE IF NOT EXISTS topics (
    openalex_id     VARCHAR PRIMARY KEY,
    display_name    VARCHAR,
    description     VARCHAR,
    keywords        VARCHAR[],
    subfield_id     VARCHAR,
    field_id        VARCHAR,
    domain_id       VARCHAR
);

CREATE TABLE IF NOT EXISTS domains (
    openalex_id     VARCHAR PRIMARY KEY,
    display_name    VARCHAR
);

CREATE TABLE IF NOT EXISTS fields (
    openalex_id     VARCHAR PRIMARY KEY,
    display_name    VARCHAR,
    domain_id       VARCHAR
);

CREATE TABLE IF NOT EXISTS subfields (
    openalex_id     VARCHAR PRIMARY KEY,
    display_name    VARCHAR,
    field_id        VARCHAR,
    domain_id       VARCHAR
);

CREATE TABLE IF NOT EXISTS sources (
    openalex_id          VARCHAR PRIMARY KEY,
    display_name         VARCHAR,
    issn_l               VARCHAR,
    issn                 VARCHAR[],
    host_organization_id VARCHAR,
    type                 VARCHAR,
    is_oa                BOOLEAN,
    is_in_doaj           BOOLEAN,
    works_count          UBIGINT,
    cited_by_count       UBIGINT
);

CREATE TABLE IF NOT EXISTS institutions (
    openalex_id     VARCHAR PRIMARY KEY,
    display_name    VARCHAR,
    country_code    VARCHAR,
    type            VARCHAR,
    ror             VARCHAR,
    works_count     UBIGINT,
    cited_by_count  UBIGINT
);

CREATE TABLE IF NOT EXISTS funders (
    openalex_id     VARCHAR PRIMARY KEY,
    display_name    VARCHAR,
    country_code    VARCHAR,
    works_count     UBIGINT,
    cited_by_count  UBIGINT
);

CREATE TABLE IF NOT EXISTS publishers (
    openalex_id     VARCHAR PRIMARY KEY,
    display_name    VARCHAR,
    works_count     UBIGINT,
    cited_by_count  UBIGINT
);

CREATE TABLE IF NOT EXISTS authors (
    openalex_id                  VARCHAR PRIMARY KEY,
    display_name                 VARCHAR,
    orcid                        VARCHAR,
    works_count                  UBIGINT,
    cited_by_count               UBIGINT,
    last_known_institution_id    VARCHAR,
    affiliations                 JSON,
    ids                          JSON
);

-- `referenced_works` / `related_works` arrays from the snapshot are normalized
-- into the `work_references` edge table during load — no list column here.
-- One path: query refs via `work_references` join.
--
-- PRIMARY KEY (openalex_id) is safe because the works loader is **streaming
-- pre-dedupe**: it walks partitions newest-first and gates each row through
-- an in-RAM seen-set keyed on integer work-ID, so no duplicate openalex_id
-- is ever appended. PK violations cannot occur on a clean stream; the only
-- cost is a B-tree append per row. Don't reintroduce ON CONFLICT here, and
-- don't drop the PK — both are symptoms of the older load-then-dedupe
-- architecture that the streaming approach replaces.
CREATE TABLE IF NOT EXISTS works (
    openalex_id         VARCHAR PRIMARY KEY,
    doi                 VARCHAR,
    title               VARCHAR,
    abstract_text       VARCHAR,
    publication_year    USMALLINT,
    publication_date    DATE,
    language            VARCHAR,
    type                VARCHAR,
    cited_by_count      UBIGINT,
    is_retracted        BOOLEAN,
    is_oa               BOOLEAN,
    oa_url              VARCHAR,
    primary_source_id   VARCHAR
);

-- No PK: `author_position` is nullable (some snapshot rows omit it) and
-- `institution_id` is frequently NULL (author with no listed institution).
-- Treat as an event/edge log; queries that need uniqueness use DISTINCT.
-- See `idx_authorships_*` indexes below for join performance.
CREATE TABLE IF NOT EXISTS work_authorships (
    work_id                VARCHAR NOT NULL,
    author_id              VARCHAR NOT NULL,
    author_position        VARCHAR,
    raw_affiliation_string VARCHAR,
    institution_id         VARCHAR
);
CREATE INDEX IF NOT EXISTS idx_authorships_work   ON work_authorships(work_id);
CREATE INDEX IF NOT EXISTS idx_authorships_author ON work_authorships(author_id);

-- Composite PKs are safe for the same reason `works` is — the streaming
-- pre-dedupe in the works loader emits each work exactly once, so its
-- edge rows are unique by construction.
CREATE TABLE IF NOT EXISTS work_topics (
    work_id   VARCHAR NOT NULL,
    topic_id  VARCHAR NOT NULL,
    score     REAL,
    PRIMARY KEY (work_id, topic_id)
);

CREATE TABLE IF NOT EXISTS work_concepts (
    work_id     VARCHAR NOT NULL,
    concept_id  VARCHAR NOT NULL,
    score       REAL,
    PRIMARY KEY (work_id, concept_id)
);

-- Outbound citation edge table: one row per (citing_work, cited_work).
-- Populated inline with works load via UNNEST(referenced_works); the
-- streaming pre-dedupe ensures each (citing_work, cited_work) pair appears
-- at most once.
CREATE TABLE IF NOT EXISTS work_references (
    work_id            VARCHAR NOT NULL,
    referenced_work_id VARCHAR NOT NULL,
    PRIMARY KEY (work_id, referenced_work_id)
);

-- Resumability: ingest tracks (entity, partition) → status so a re-run skips
-- already-loaded partitions.
CREATE TABLE IF NOT EXISTS _ingest_log (
    entity      VARCHAR NOT NULL,
    partition   VARCHAR NOT NULL,
    status      VARCHAR NOT NULL,        -- 'ok' | 'error'
    rows        UBIGINT,
    parse_errors UBIGINT,
    completed_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (entity, partition)
);

-- Per-component readiness sentinel. A row here means that component's
-- end-to-end pipeline (load + indexes + FTS where applicable) finished
-- successfully and the tables are usable for queries. hs-mcp gates the
-- visibility of `openalex_*` MCP tools on `_corpus_state.component =
-- 'openalex_works'` — absent row means "corpus not ready, hide the tools".
-- Inserted at the end of `OpenAlexDb::build_fts`; nothing else writes here.
CREATE TABLE IF NOT EXISTS _corpus_state (
    component   VARCHAR PRIMARY KEY,
    ready_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    notes       VARCHAR
);
"#;

/// Secondary indexes built after the core load. Cheap on small tables, slow
/// on `works` and `work_references` so they live in their own DDL block to be
/// triggered explicitly.
///
/// Primary keys are baked into the table DDL (see `SCHEMA_DDL`) and are
/// applied at table-create time. There is no separate "post-dedupe unique
/// index" step — the streaming pre-dedupe in the works loader guarantees
/// the PKs are never violated at insert time.
pub const POST_LOAD_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_works_doi              ON works(doi);
CREATE INDEX IF NOT EXISTS idx_works_year             ON works(publication_year);
CREATE INDEX IF NOT EXISTS idx_works_cited_by         ON works(cited_by_count);
CREATE INDEX IF NOT EXISTS idx_work_topics_topic      ON work_topics(topic_id);
CREATE INDEX IF NOT EXISTS idx_work_concepts_concept  ON work_concepts(concept_id);
CREATE INDEX IF NOT EXISTS idx_refs_referenced        ON work_references(referenced_work_id);
"#;
