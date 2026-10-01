//! What a distill Qdrant collection must look like, and the pure decision of
//! what to do about a collection that already exists.
//!
//! Kept free of Qdrant types so the schema rules are unit-testable: the
//! `qdrant` module translates a live `CollectionInfo` into [`Observed`] and
//! carries out the [`SchemaPlan`].

use std::collections::BTreeMap;

use crate::config::HnswConfig;
use crate::error::DistillError;

/// Kind of payload index a field needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Keyword,
    Integer,
    Text,
}

/// Payload fields the server filters or facets on. Every one must be
/// indexed in every served collection; startup creates any that is missing.
///
/// Deliberately NOT here: `chunk_index`. Stale-tail deletion filters on
/// `doc_id` (indexed) and `chunk_index >= n`; Qdrant checks the second
/// condition against the few points the first selects, so no index is
/// needed, and building a new one over a multi-million-point collection
/// inside a service restart would run past the startup timeout.
pub const REQUIRED_INDEXES: [(&str, FieldKind); 11] = [
    ("doc_id", FieldKind::Keyword),
    ("authors", FieldKind::Keyword),
    ("topics", FieldKind::Keyword),
    ("keywords", FieldKind::Keyword),
    ("pdf_path", FieldKind::Keyword),
    ("category", FieldKind::Keyword),
    ("original_format", FieldKind::Keyword),
    ("year", FieldKind::Integer),
    ("line_start", FieldKind::Integer),
    ("page", FieldKind::Integer),
    ("title", FieldKind::Text),
];

/// The schema a collection is created with and checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionSpec {
    /// Vector width — the embedder's measured output length.
    pub dimension: usize,
    pub hnsw_m: u64,
    pub hnsw_ef_construct: u64,
}

impl CollectionSpec {
    pub fn new(dimension: usize, hnsw: &HnswConfig) -> Self {
        Self {
            dimension,
            hnsw_m: hnsw.m,
            hnsw_ef_construct: hnsw.ef_construct,
        }
    }
}

/// What a live collection reports, reduced to the facts the plan needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Width of the single unnamed vector; `None` when the collection uses
    /// named vectors or reports none.
    pub vector_size: Option<u64>,
    /// Whether that vector uses cosine distance.
    pub cosine: Option<bool>,
    /// Effective HNSW `m` (vector-level override, else collection-level).
    pub hnsw_m: Option<u64>,
    /// Payload field -> index kind, `None` for a kind this server does not
    /// create (float, geo, ...).
    pub indexes: BTreeMap<String, Option<FieldKind>>,
}

/// What to do to bring a collection in line with its spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaPlan {
    /// Required indexes that do not exist yet. Creating them is idempotent,
    /// so this runs on every start and repairs a collection whose creation
    /// was interrupted after the collection itself existed.
    pub create_indexes: Vec<(&'static str, FieldKind)>,
    /// HNSW is off (`m == 0`): every query is a brute-force scan. All
    /// collections created before RA-41 are in this state.
    pub hnsw_disabled: bool,
}

/// Compare a live collection with its spec. A collection that cannot hold
/// this server's vectors (wrong width or distance), or that indexes a
/// required field with the wrong type, is an error — never accepted
/// because its name matched.
pub fn plan_collection(
    name: &str,
    observed: &Observed,
    spec: &CollectionSpec,
) -> Result<SchemaPlan, DistillError> {
    match observed.vector_size {
        Some(size) if size == spec.dimension as u64 => {}
        Some(size) => {
            return Err(DistillError::Qdrant(format!(
                "collection '{name}' holds {size}-wide vectors but the embedder produces {}-wide; \
                 recreate the collection (hs pipeline rebuild) or fix embedding.dimension",
                spec.dimension
            )))
        }
        None => {
            return Err(DistillError::Qdrant(format!(
                "collection '{name}' has no single unnamed vector config (named vectors?); \
                 distill only reads and writes the default vector"
            )))
        }
    }
    if observed.cosine != Some(true) {
        return Err(DistillError::Qdrant(format!(
            "collection '{name}' does not use cosine distance; distill scores with cosine"
        )));
    }

    let mut create_indexes = Vec::new();
    for (field, kind) in REQUIRED_INDEXES {
        match observed.indexes.get(field) {
            None => create_indexes.push((field, kind)),
            Some(Some(existing)) if *existing == kind => {}
            Some(existing) => {
                return Err(DistillError::Qdrant(format!(
                    "collection '{name}' indexes payload field '{field}' as {existing:?} but \
                     distill needs {kind:?}; delete that payload index and restart"
                )))
            }
        }
    }

    Ok(SchemaPlan {
        create_indexes,
        hnsw_disabled: observed.hnsw_m == Some(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CollectionSpec {
        CollectionSpec::new(1024, &HnswConfig::default())
    }

    fn healthy() -> Observed {
        Observed {
            vector_size: Some(1024),
            cosine: Some(true),
            hnsw_m: Some(16),
            indexes: REQUIRED_INDEXES
                .iter()
                .map(|(f, k)| (f.to_string(), Some(*k)))
                .collect(),
        }
    }

    #[test]
    fn a_complete_collection_needs_nothing() {
        let plan = plan_collection("c", &healthy(), &spec()).unwrap();
        assert!(plan.create_indexes.is_empty());
        assert!(!plan.hnsw_disabled);
    }

    #[test]
    fn a_fresh_collection_gets_every_index() {
        let fresh = Observed {
            indexes: BTreeMap::new(),
            ..healthy()
        };
        let plan = plan_collection("c", &fresh, &spec()).unwrap();
        assert_eq!(plan.create_indexes.len(), REQUIRED_INDEXES.len());
    }

    #[test]
    fn an_interrupted_creation_is_repaired_not_accepted() {
        // Collection exists, but the process died after create_collection
        // and before every index: the next start must create the rest.
        let mut partial = healthy();
        partial.indexes.remove("year");
        partial.indexes.remove("title");
        let plan = plan_collection("c", &partial, &spec()).unwrap();
        assert_eq!(
            plan.create_indexes,
            [("year", FieldKind::Integer), ("title", FieldKind::Text)]
        );
    }

    #[test]
    fn extra_indexes_are_left_alone() {
        let mut o = healthy();
        o.indexes.insert("something_else".into(), None);
        assert!(plan_collection("c", &o, &spec())
            .unwrap()
            .create_indexes
            .is_empty());
    }

    #[test]
    fn wrong_vector_width_is_an_error() {
        let mut o = healthy();
        o.vector_size = Some(768);
        let err = plan_collection("c", &o, &spec()).unwrap_err().to_string();
        assert!(err.contains("768") && err.contains("1024"), "{err}");
    }

    #[test]
    fn named_or_missing_vector_config_is_an_error() {
        let mut o = healthy();
        o.vector_size = None;
        assert!(plan_collection("c", &o, &spec()).is_err());
    }

    #[test]
    fn non_cosine_distance_is_an_error() {
        for cosine in [Some(false), None] {
            let mut o = healthy();
            o.cosine = cosine;
            assert!(plan_collection("c", &o, &spec()).is_err());
        }
    }

    #[test]
    fn an_index_of_the_wrong_type_is_an_error() {
        let mut o = healthy();
        o.indexes.insert("year".into(), Some(FieldKind::Keyword));
        let err = plan_collection("c", &o, &spec()).unwrap_err().to_string();
        assert!(err.contains("year"), "{err}");

        let mut o = healthy();
        o.indexes.insert("doc_id".into(), None);
        assert!(plan_collection("c", &o, &spec()).is_err());
    }

    #[test]
    fn disabled_hnsw_is_reported_but_not_fatal() {
        let mut o = healthy();
        o.hnsw_m = Some(0);
        let plan = plan_collection("c", &o, &spec()).unwrap();
        assert!(plan.hnsw_disabled);

        o.hnsw_m = None;
        assert!(!plan_collection("c", &o, &spec()).unwrap().hnsw_disabled);
    }
}
