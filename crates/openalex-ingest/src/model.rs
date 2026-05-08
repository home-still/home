//! Serde structs for OpenAlex JSONL snapshot entities.
//!
//! Field set is intentionally narrow — only what we persist. Unknown fields are
//! dropped by serde's default behavior. Vec fields use `null_as_default` because
//! the snapshot routinely emits `null` (not `[]` and not "missing") for empty
//! arrays — observed on `authorships`, `topics`, `concepts`, `referenced_works`,
//! `affiliations`, etc.

use serde::{Deserialize, Deserializer};
use std::collections::HashMap;

/// Coerce JSON `null` (or absent) to `T::default()` instead of erroring.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

#[derive(Debug, Deserialize)]
pub struct Work {
    pub id: String,
    pub doi: Option<String>,
    pub title: Option<String>,
    pub display_name: Option<String>,
    pub publication_year: Option<u32>,
    pub publication_date: Option<String>,
    pub language: Option<String>,
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    pub cited_by_count: Option<u64>,
    pub is_retracted: Option<bool>,
    pub abstract_inverted_index: Option<HashMap<String, Vec<u32>>>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub authorships: Vec<Authorship>,
    pub primary_location: Option<Location>,
    pub open_access: Option<OpenAccess>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub topics: Vec<TopicRef>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub concepts: Vec<ConceptRef>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub referenced_works: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub related_works: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Authorship {
    pub author: AuthorRef,
    pub author_position: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub raw_affiliation_strings: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub institutions: Vec<InstitutionRef>,
}

#[derive(Debug, Deserialize)]
pub struct AuthorRef {
    pub id: Option<String>,
    pub display_name: Option<String>,
    pub orcid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InstitutionRef {
    pub id: Option<String>,
    pub display_name: Option<String>,
    pub country_code: Option<String>,
    #[serde(rename = "type")]
    pub inst_type: Option<String>,
    pub ror: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Location {
    pub source: Option<SourceRef>,
    pub is_oa: Option<bool>,
    pub landing_page_url: Option<String>,
    pub pdf_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SourceRef {
    pub id: Option<String>,
    pub display_name: Option<String>,
    pub issn_l: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub issn: Vec<String>,
    pub host_organization: Option<String>,
    #[serde(rename = "type")]
    pub source_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAccess {
    pub is_oa: Option<bool>,
    pub oa_url: Option<String>,
    pub oa_status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TopicRef {
    pub id: String,
    pub display_name: Option<String>,
    pub score: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct ConceptRef {
    pub id: String,
    pub display_name: Option<String>,
    pub level: Option<u8>,
    pub score: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct Author {
    pub id: String,
    pub display_name: Option<String>,
    pub orcid: Option<String>,
    pub works_count: Option<u64>,
    pub cited_by_count: Option<u64>,
    /// Snapshot field is `last_known_institutions` (plural list), NOT
    /// `last_known_institution` (singular) as some live-API docs imply.
    #[serde(default, deserialize_with = "null_as_default")]
    pub last_known_institutions: Vec<InstitutionRef>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub affiliations: Vec<serde_json::Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub ids: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct Source {
    pub id: String,
    pub display_name: Option<String>,
    pub issn_l: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub issn: Vec<String>,
    pub host_organization: Option<String>,
    #[serde(rename = "type")]
    pub source_type: Option<String>,
    pub is_oa: Option<bool>,
    pub is_in_doaj: Option<bool>,
    pub works_count: Option<u64>,
    pub cited_by_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Institution {
    pub id: String,
    pub display_name: Option<String>,
    pub country_code: Option<String>,
    #[serde(rename = "type")]
    pub inst_type: Option<String>,
    pub ror: Option<String>,
    pub works_count: Option<u64>,
    pub cited_by_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Topic {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub keywords: Vec<String>,
    pub subfield: Option<TaxonomyNode>,
    pub field: Option<TaxonomyNode>,
    pub domain: Option<TaxonomyNode>,
}

#[derive(Debug, Deserialize)]
pub struct TaxonomyNode {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Concept {
    pub id: String,
    pub display_name: Option<String>,
    pub level: Option<u8>,
    pub description: Option<String>,
    pub wikidata: Option<String>,
    pub works_count: Option<u64>,
    pub cited_by_count: Option<u64>,
    pub ancestors: Option<Vec<ConceptAncestor>>,
}

#[derive(Debug, Deserialize)]
pub struct ConceptAncestor {
    pub id: String,
    pub display_name: Option<String>,
    pub level: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct Funder {
    pub id: String,
    pub display_name: Option<String>,
    pub country_code: Option<String>,
    pub works_count: Option<u64>,
    pub cited_by_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Publisher {
    pub id: String,
    pub display_name: Option<String>,
    pub works_count: Option<u64>,
    pub cited_by_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Domain {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Field {
    pub id: String,
    pub display_name: Option<String>,
    pub domain: Option<TaxonomyNode>,
}

#[derive(Debug, Deserialize)]
pub struct Subfield {
    pub id: String,
    pub display_name: Option<String>,
    pub field: Option<TaxonomyNode>,
    pub domain: Option<TaxonomyNode>,
}
