//! The stores of the corpus: one registry that every crate reads.
//!
//! A *store* is a named body of text an agent can search: the academic
//! papers, the personal documents, captured web pages, saved syntheses,
//! the reference lists and abstracts derived from the papers, and what
//! agents were asked to remember. Each store has one name (what an agent
//! and a document link say), one tenant (where its records live in the
//! memory ledger) and, when it holds documents, one layout (where its
//! objects live in the corpus's storage).
//!
//! This registry is where a store's name, tenant and layout are defined:
//! the docs service resolves a link's store here, the MCP server checks a
//! `store` argument here, and the pipeline finds a source object's store
//! here. A name the registry does not hold is an error at the boundary
//! that received it.

use std::fmt;

use serde::Deserialize;

use crate::config_file::{ConfigError, ConfigFile};

/// The academic papers.
pub const ACADEMIC: &str = "academic";
/// The reference lists of the academic papers.
pub const REFERENCES: &str = "references";
/// One record per academic paper: its title and abstract.
pub const ABSTRACTS: &str = "abstracts";
/// The owner's personal documents.
pub const PERSONAL: &str = "personal";
/// Web pages an agent captured.
pub const WEB: &str = "web";
/// Answers an agent saved, with the quotes it verified.
pub const SYNTHESES: &str = "syntheses";
/// What agents were asked to remember.
pub const MEMORY: &str = "memory";

/// The prefix the academic sources live under.
pub const ACADEMIC_SOURCE_PREFIX: &str = "papers";
/// The prefix the academic markdown lives under.
pub const ACADEMIC_MARKDOWN_PREFIX: &str = "markdown";
/// The prefix the academic catalog entries live under.
pub const ACADEMIC_CATALOG_PREFIX: &str = "catalog";
/// The prefix the academic word-box sidecars live under.
pub const ACADEMIC_BOXES_PREFIX: &str = "boxes";

/// The personal store's prefix when `personal.storage_dir` is not set.
pub const PERSONAL_PREFIX_DEFAULT: &str = "personal";
/// The environment variable that overrides `personal.storage_dir`, the
/// same one the personal store's own loader maps to that key.
pub const PERSONAL_STORAGE_DIR_ENV: &str = "HOME_STILL_PERSONAL_STORAGE_DIR";

/// The prefix the captured web pages live under.
const WEB_PREFIX: &str = "web";
/// The prefix the saved syntheses live under.
const SYNTHESES_PREFIX: &str = "syntheses";

/// Under a nested store's own prefix: where its markdown lives.
const NESTED_MARKDOWN_DIR: &str = "markdown";
/// Under a nested store's own prefix: where its catalog entries live.
const NESTED_CATALOG_DIR: &str = "catalog";
/// Under a nested store's own prefix: where its word-box sidecars live.
const NESTED_BOXES_DIR: &str = "boxes";

/// The extensions an academic source may have, in probe order.
const ACADEMIC_SOURCE_EXTENSIONS: &[&str] = &["pdf", "html", "epub"];
/// The extensions a personal source may have, in probe order: every
/// format the personal store accepts (`personal::models::SourceFormat`).
const PERSONAL_SOURCE_EXTENSIONS: &[&str] = &["pdf", "epub", "docx", "md", "txt"];
/// The extensions a captured web page may have, in probe order.
const WEB_SOURCE_EXTENSIONS: &[&str] = &["html", "md", "txt"];
/// The extension a saved synthesis has.
const SYNTHESES_SOURCE_EXTENSIONS: &[&str] = &["md"];

/// One store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSpec {
    /// The store's name: what an agent passes as `store` and what a
    /// document link carries as `col`.
    pub name: &'static str,
    /// The memory ledger's tenant that holds the store's records.
    pub tenant: &'static str,
    /// One sentence for an agent choosing which stores to read.
    pub description: &'static str,
    /// Whether a read that names no store covers this one.
    pub default_read: bool,
    /// Whether an agent may write to it with `remember`.
    pub agent_writes: bool,
    /// Whether its records carry the time they became true.
    pub dated: bool,
    /// What the store holds.
    pub kind: StoreKind,
}

/// What a store holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreKind {
    /// Documents of its own, at this layout in the corpus's storage.
    Documents(DocumentLayout),
    /// Text taken from another store's documents. The documents, and so
    /// the links, are the parent's.
    Derived {
        /// The name of the store whose documents these records come from.
        of: &'static str,
        /// Which text of the parent's documents the records are.
        text: DerivedText,
    },
    /// Statements with no document behind them.
    Memory,
}

/// Which text of the parent's documents a derived store holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedText {
    /// The chunks of a document that are its reference list.
    ReferenceChunks,
    /// The abstract in the document's catalog entry.
    CatalogAbstract,
}

/// Where one store's objects live in the corpus's storage.
///
/// No prefix ends in `/`. Under each one the objects are sharded the way
/// [`crate::sharded_key`] shards them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentLayout {
    /// The prefix the source documents live under.
    pub source_prefix: String,
    /// The prefix the converted markdown lives under.
    pub markdown_prefix: String,
    /// The prefix the catalog entries live under.
    pub catalog_prefix: String,
    /// The prefix the word-box sidecars live under.
    pub boxes_prefix: String,
    /// The extensions a source document may have, in probe order.
    pub source_extensions: &'static [&'static str],
}

impl DocumentLayout {
    /// The academic corpus: four sibling prefixes at the top of the
    /// storage.
    fn academic() -> Self {
        Self {
            source_prefix: ACADEMIC_SOURCE_PREFIX.to_string(),
            markdown_prefix: ACADEMIC_MARKDOWN_PREFIX.to_string(),
            catalog_prefix: ACADEMIC_CATALOG_PREFIX.to_string(),
            boxes_prefix: ACADEMIC_BOXES_PREFIX.to_string(),
            source_extensions: ACADEMIC_SOURCE_EXTENSIONS,
        }
    }

    /// A store kept under one prefix of its own: the sources directly
    /// under `root`, everything made from them one directory deeper.
    fn nested(root: &str, source_extensions: &'static [&'static str]) -> Self {
        Self {
            source_prefix: root.to_string(),
            markdown_prefix: format!("{root}/{NESTED_MARKDOWN_DIR}"),
            catalog_prefix: format!("{root}/{NESTED_CATALOG_DIR}"),
            boxes_prefix: format!("{root}/{NESTED_BOXES_DIR}"),
            source_extensions,
        }
    }

    /// The key a stem's source would have with this extension.
    ///
    /// Does not validate `stem`; call [`crate::validate_stem`] first on
    /// untrusted input.
    pub fn source_key(&self, stem: &str, extension: &str) -> String {
        format!(
            "{}/{}",
            self.source_prefix,
            crate::sharded_key(stem, extension)
        )
    }

    /// The key of a stem's converted markdown.
    ///
    /// Does not validate `stem`; call [`crate::validate_stem`] first on
    /// untrusted input.
    pub fn markdown_key(&self, stem: &str) -> String {
        format!(
            "{}/{}",
            self.markdown_prefix,
            crate::sharded_key(stem, MARKDOWN_EXTENSION)
        )
    }

    /// Every prefix of the layout.
    fn prefixes(&self) -> [&str; 4] {
        [
            &self.source_prefix,
            &self.markdown_prefix,
            &self.catalog_prefix,
            &self.boxes_prefix,
        ]
    }

    /// Whether `key` is one of this layout's source objects: under the
    /// source prefix and not under a prefix of what is made from the
    /// sources (a nested store keeps those below its source prefix).
    fn holds_source(&self, key: &str) -> bool {
        is_under(key, &self.source_prefix)
            && ![
                &self.markdown_prefix,
                &self.catalog_prefix,
                &self.boxes_prefix,
            ]
            .into_iter()
            .any(|made| is_under(key, made))
    }
}

/// The extension of a converted markdown object.
const MARKDOWN_EXTENSION: &str = "md";

/// Whether `key` is below the directory `prefix`.
fn is_under(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether one prefix is the other or a directory inside it.
fn prefixes_overlap(a: &str, b: &str) -> bool {
    a == b || is_under(a, b) || is_under(b, a)
}

/// A store name, tenant or use the registry cannot satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownStore {
    /// No store has this name.
    Name {
        /// The name that was asked for.
        asked: String,
        /// The names that exist.
        known: Vec<&'static str>,
    },
    /// No store has this tenant.
    Tenant {
        /// The tenant that was asked for.
        asked: String,
    },
    /// The store exists and holds no documents.
    NoDocuments {
        /// The store that was asked for.
        name: &'static str,
    },
}

impl fmt::Display for UnknownStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name { asked, known } => write!(
                f,
                "no store named {asked:?}; the stores are {}",
                known.join(", ")
            ),
            Self::Tenant { asked } => write!(f, "no store has the tenant {asked:?}"),
            Self::NoDocuments { name } => write!(f, "the store {name:?} holds no documents"),
        }
    }
}

impl std::error::Error for UnknownStore {}

/// Why the registry could not be built from the configuration.
#[derive(Debug)]
pub enum StoreConfigError {
    /// A config file could not be read, or its `personal:` section does
    /// not fit.
    Config(ConfigError),
    /// A store's prefix is not a plain relative path.
    InvalidPrefix {
        /// The store the prefix belongs to.
        store: &'static str,
        /// The prefix as configured.
        prefix: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Two stores would keep objects in the same place.
    Overlap {
        /// One store and its prefix.
        store: &'static str,
        /// That store's prefix.
        prefix: String,
        /// The other store.
        other_store: &'static str,
        /// The other store's prefix.
        other_prefix: String,
    },
}

impl fmt::Display for StoreConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(e) => e.fmt(f),
            Self::InvalidPrefix {
                store,
                prefix,
                reason,
            } => write!(
                f,
                "the {store} store's prefix {prefix:?} is not usable: {reason}"
            ),
            Self::Overlap {
                store,
                prefix,
                other_store,
                other_prefix,
            } => write!(
                f,
                "the {store} store's prefix {prefix:?} overlaps the {other_store} store's \
                 {other_prefix:?}: two stores cannot keep objects in the same place"
            ),
        }
    }
}

impl std::error::Error for StoreConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(e) => Some(e),
            Self::InvalidPrefix { .. } | Self::Overlap { .. } => None,
        }
    }
}

impl From<ConfigError> for StoreConfigError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

/// The one key of the `personal:` section the registry reads. The
/// section's other keys belong to the personal store's own loader.
#[derive(Debug, Deserialize)]
struct PersonalSection {
    storage_dir: Option<String>,
}

/// The personal store's prefix in the corpus's storage: the environment
/// variable [`PERSONAL_STORAGE_DIR_ENV`], else `personal.storage_dir` in
/// the user's file, else in the system-wide file, else
/// [`PERSONAL_PREFIX_DEFAULT`]. Checked, and without a trailing `/`.
///
/// The personal store's loader takes its prefix from here, so the store
/// that writes the documents and the services that read them cannot
/// disagree about where they are.
pub fn personal_prefix(system: &ConfigFile, user: &ConfigFile) -> Result<String, StoreConfigError> {
    resolve_personal_prefix(system, user, std::env::var(PERSONAL_STORAGE_DIR_ENV).ok())
}

/// [`personal_prefix`] with the environment's value passed in.
fn resolve_personal_prefix(
    system: &ConfigFile,
    user: &ConfigFile,
    from_env: Option<String>,
) -> Result<String, StoreConfigError> {
    let configured = match from_env {
        Some(value) => Some(value),
        None => {
            let mut found = None;
            for file in [user, system] {
                let section = file.section::<PersonalSection>("personal")?;
                if let Some(dir) = section.and_then(|s| s.storage_dir) {
                    found = Some(dir);
                    break;
                }
            }
            found
        }
    };
    let prefix = configured.unwrap_or_else(|| PERSONAL_PREFIX_DEFAULT.to_string());
    let prefix = prefix.trim_end_matches('/');
    check_prefix(PERSONAL, prefix)?;
    Ok(prefix.to_string())
}

/// Check that `prefix` is a plain relative path: storage keys are built
/// by joining onto it, so an empty, absolute or `..`-bearing prefix would
/// put a store's objects outside the place it names.
fn check_prefix(store: &'static str, prefix: &str) -> Result<(), StoreConfigError> {
    let reason = if prefix.is_empty() {
        Some("it is empty")
    } else if prefix.contains(['\\', '\0']) {
        Some("it contains a backslash or a NUL byte")
    } else if prefix.starts_with('/') {
        Some("it is absolute; a prefix is relative to the storage root")
    } else if prefix
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        Some("it has an empty, `.` or `..` path segment")
    } else {
        None
    };
    match reason {
        None => Ok(()),
        Some(reason) => Err(StoreConfigError::InvalidPrefix {
            store,
            prefix: prefix.to_string(),
            reason,
        }),
    }
}

/// The registry.
#[derive(Debug, Clone)]
pub struct Stores(Vec<StoreSpec>);

impl Stores {
    /// Build the registry from this machine's configuration: the user's
    /// config file, the system-wide one and the environment.
    pub fn load() -> Result<Self, StoreConfigError> {
        let user = ConfigFile::load()?;
        let system = user.load_system()?;
        Self::from_config(&system, &user)
    }

    /// Build the registry from already-read config files (and the
    /// environment). The only configured part is the personal store's
    /// prefix: see [`personal_prefix`].
    pub fn from_config(system: &ConfigFile, user: &ConfigFile) -> Result<Self, StoreConfigError> {
        Self::with_personal_prefix(&personal_prefix(system, user)?)
    }

    /// Build the registry with the personal store at `personal_prefix`.
    pub fn with_personal_prefix(personal_prefix: &str) -> Result<Self, StoreConfigError> {
        let personal_prefix = personal_prefix.trim_end_matches('/');
        check_prefix(PERSONAL, personal_prefix)?;
        let stores = Self(vec![
            StoreSpec {
                name: ACADEMIC,
                tenant: "academic_papers",
                description: "The full text of the academic papers in the corpus, in passages \
                              that carry their page and lines.",
                default_read: true,
                agent_writes: false,
                dated: false,
                kind: StoreKind::Documents(DocumentLayout::academic()),
            },
            StoreSpec {
                name: REFERENCES,
                tenant: "academic_references",
                description: "The reference lists of the academic papers: read it to find what \
                              a paper cites. Kept out of `academic` so bibliographies do not \
                              crowd out prose.",
                default_read: false,
                agent_writes: false,
                dated: false,
                kind: StoreKind::Derived {
                    of: ACADEMIC,
                    text: DerivedText::ReferenceChunks,
                },
            },
            StoreSpec {
                name: ABSTRACTS,
                tenant: "paper_abstracts",
                description: "One record per academic paper, its title and abstract: read it to \
                              find papers on a topic rather than passages.",
                default_read: false,
                agent_writes: false,
                dated: false,
                kind: StoreKind::Derived {
                    of: ACADEMIC,
                    text: DerivedText::CatalogAbstract,
                },
            },
            StoreSpec {
                name: PERSONAL,
                tenant: "personal_docs",
                description: "The owner's personal documents (medical, financial, legal, \
                              notes). Private: read it only when the question is about the \
                              owner.",
                default_read: false,
                agent_writes: false,
                dated: false,
                // The personal store's ingest (`personal::services::ingest`)
                // keeps each document's card in a `<stem>.catalog.yaml`
                // sidecar beside the source, read through
                // `personal::services::catalog`; it writes nothing under
                // this layout's `catalog_prefix`.
                kind: StoreKind::Documents(DocumentLayout::nested(
                    personal_prefix,
                    PERSONAL_SOURCE_EXTENSIONS,
                )),
            },
            StoreSpec {
                name: WEB,
                tenant: "web_pages",
                description: "Web pages an agent captured, each with the address it was \
                              captured from.",
                default_read: false,
                agent_writes: false,
                dated: false,
                kind: StoreKind::Documents(DocumentLayout::nested(
                    WEB_PREFIX,
                    WEB_SOURCE_EXTENSIONS,
                )),
            },
            StoreSpec {
                name: SYNTHESES,
                tenant: "syntheses",
                description: "Answers an agent saved, each with the quotes it checked against \
                              their sources.",
                default_read: false,
                agent_writes: false,
                dated: false,
                kind: StoreKind::Documents(DocumentLayout::nested(
                    SYNTHESES_PREFIX,
                    SYNTHESES_SOURCE_EXTENSIONS,
                )),
            },
            StoreSpec {
                name: MEMORY,
                tenant: "user_memory",
                description: "What agents were asked to remember about the user and past work: \
                              preferences, decisions and facts, each with its date.",
                default_read: false,
                agent_writes: true,
                dated: true,
                kind: StoreKind::Memory,
            },
        ]);
        stores.check_disjoint()?;
        Ok(stores)
    }

    /// No two stores may keep objects in the same place: a prefix of one
    /// that equals, contains or sits inside a prefix of another would let
    /// one store's listing return the other's objects.
    fn check_disjoint(&self) -> Result<(), StoreConfigError> {
        let layouts: Vec<(&StoreSpec, &DocumentLayout)> = self
            .0
            .iter()
            .filter_map(|spec| match &spec.kind {
                StoreKind::Documents(layout) => Some((spec, layout)),
                StoreKind::Derived { .. } | StoreKind::Memory => None,
            })
            .collect();
        for (i, (spec, layout)) in layouts.iter().enumerate() {
            for (other_spec, other_layout) in &layouts[i + 1..] {
                for prefix in layout.prefixes() {
                    for other_prefix in other_layout.prefixes() {
                        if prefixes_overlap(prefix, other_prefix) {
                            // Name the configured store first: its prefix is
                            // the one an operator can change.
                            let (a, a_prefix, b, b_prefix) = if other_spec.name == PERSONAL {
                                (other_spec.name, other_prefix, spec.name, prefix)
                            } else {
                                (spec.name, prefix, other_spec.name, other_prefix)
                            };
                            return Err(StoreConfigError::Overlap {
                                store: a,
                                prefix: a_prefix.to_string(),
                                other_store: b,
                                other_prefix: b_prefix.to_string(),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The store with this name.
    pub fn get(&self, name: &str) -> Result<&StoreSpec, UnknownStore> {
        self.0
            .iter()
            .find(|spec| spec.name == name)
            .ok_or_else(|| UnknownStore::Name {
                asked: name.to_string(),
                known: self.0.iter().map(|spec| spec.name).collect(),
            })
    }

    /// The store whose records live in this tenant of the memory ledger.
    pub fn by_tenant(&self, tenant: &str) -> Result<&StoreSpec, UnknownStore> {
        self.0
            .iter()
            .find(|spec| spec.tenant == tenant)
            .ok_or_else(|| UnknownStore::Tenant {
                asked: tenant.to_string(),
            })
    }

    /// The store that holds the documents behind the store named `name`,
    /// and where they are: the store itself, or for a derived store the
    /// one it is derived from. A store with no documents is an error.
    pub fn documents(&self, name: &str) -> Result<(&StoreSpec, &DocumentLayout), UnknownStore> {
        let spec = self.get(name)?;
        let holder = match &spec.kind {
            StoreKind::Derived { of, .. } => self.get(of)?,
            StoreKind::Documents(_) | StoreKind::Memory => spec,
        };
        match &holder.kind {
            StoreKind::Documents(layout) => Ok((holder, layout)),
            StoreKind::Derived { .. } | StoreKind::Memory => {
                Err(UnknownStore::NoDocuments { name: spec.name })
            }
        }
    }

    /// The store a source object's storage key belongs to. `None` for a
    /// key under no store's source prefix, and for a key under what is
    /// made from the sources (markdown, catalog entries, word boxes).
    ///
    /// The stores' prefixes are disjoint (checked when the registry is
    /// built), so at most one store holds a key.
    pub fn for_source_key(&self, key: &str) -> Option<(&StoreSpec, &DocumentLayout)> {
        self.0.iter().find_map(|spec| match &spec.kind {
            StoreKind::Documents(layout) if layout.holds_source(key) => Some((spec, layout)),
            StoreKind::Documents(_) | StoreKind::Derived { .. } | StoreKind::Memory => None,
        })
    }

    /// Every store, in the order an agent is shown them.
    pub fn iter(&self) -> impl Iterator<Item = &StoreSpec> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stores() -> Stores {
        Stores::with_personal_prefix(PERSONAL_PREFIX_DEFAULT).unwrap()
    }

    /// A config file in a fresh directory, holding `yaml`.
    fn config(dir: &std::path::Path, name: &str, yaml: &str) -> ConfigFile {
        let path = dir.join(name);
        std::fs::write(&path, yaml).unwrap();
        ConfigFile::load_at(&path, dir).unwrap()
    }

    /// The tenants are where the records already are in the ledger:
    /// renaming one would orphan every record under the old name.
    #[test]
    fn each_store_keeps_its_ledger_tenant() {
        let stores = stores();
        let tenants: Vec<(&str, &str)> = stores.iter().map(|s| (s.name, s.tenant)).collect();
        assert_eq!(
            tenants,
            vec![
                ("academic", "academic_papers"),
                ("references", "academic_references"),
                ("abstracts", "paper_abstracts"),
                ("personal", "personal_docs"),
                ("web", "web_pages"),
                ("syntheses", "syntheses"),
                ("memory", "user_memory"),
            ]
        );
        for spec in stores.iter() {
            assert_eq!(stores.by_tenant(spec.tenant).unwrap().name, spec.name);
        }
    }

    /// A read that names no store covers the papers and nothing private,
    /// and `remember` can only write to memory.
    #[test]
    fn only_the_papers_are_read_by_default_and_only_memory_is_writable() {
        let stores = stores();
        let default_read: Vec<&str> = stores
            .iter()
            .filter(|s| s.default_read)
            .map(|s| s.name)
            .collect();
        assert_eq!(default_read, vec![ACADEMIC]);
        let writable: Vec<&str> = stores
            .iter()
            .filter(|s| s.agent_writes)
            .map(|s| s.name)
            .collect();
        assert_eq!(writable, vec![MEMORY]);
    }

    #[test]
    fn a_derived_store_resolves_to_the_documents_of_its_parent() {
        let stores = stores();
        for derived in [REFERENCES, ABSTRACTS] {
            let (holder, layout) = stores.documents(derived).unwrap();
            assert_eq!(holder.name, ACADEMIC);
            assert_eq!(layout.source_prefix, "papers");
        }
        // Every derived store's parent holds documents, so `documents`
        // never has to follow more than one step.
        for spec in stores.iter() {
            if let StoreKind::Derived { of, .. } = &spec.kind {
                assert!(matches!(
                    stores.get(of).unwrap().kind,
                    StoreKind::Documents(_)
                ));
            }
        }
    }

    #[test]
    fn memory_has_no_documents_and_an_unknown_name_lists_the_stores() {
        let stores = stores();
        assert_eq!(
            stores.documents(MEMORY).unwrap_err(),
            UnknownStore::NoDocuments { name: MEMORY }
        );
        let err = stores.get("papers").unwrap_err().to_string();
        assert!(err.contains("\"papers\""), "{err}");
        for spec in stores.iter() {
            assert!(err.contains(spec.name), "{err}");
        }
        assert!(stores.by_tenant("academic").is_err());
    }

    #[test]
    fn the_layouts_put_each_store_where_its_objects_are() {
        let stores = stores();
        let (_, academic) = stores.documents(ACADEMIC).unwrap();
        assert_eq!(academic.source_key("abcd", "pdf"), "papers/ab/abcd.pdf");
        assert_eq!(academic.markdown_key("abcd"), "markdown/ab/abcd.md");
        assert_eq!(academic.catalog_prefix, "catalog");
        assert_eq!(academic.boxes_prefix, "boxes");

        let (_, personal) = stores.documents(PERSONAL).unwrap();
        assert_eq!(personal.source_key("abcd", "docx"), "personal/ab/abcd.docx");
        assert_eq!(
            personal.markdown_key("abcd"),
            "personal/markdown/ab/abcd.md"
        );
        assert_eq!(personal.boxes_prefix, "personal/boxes");

        let (_, web) = stores.documents(WEB).unwrap();
        assert_eq!(web.markdown_key("abcd"), "web/markdown/ab/abcd.md");
        assert_eq!(web.catalog_prefix, "web/catalog");
    }

    #[test]
    fn a_source_key_names_its_store() {
        let stores = stores();
        let store_of = |key: &str| stores.for_source_key(key).map(|(spec, _)| spec.name);
        assert_eq!(store_of("papers/ab/abcd.pdf"), Some(ACADEMIC));
        assert_eq!(store_of("personal/ab/abcd.docx"), Some(PERSONAL));
        assert_eq!(store_of("web/ab/abcd.html"), Some(WEB));
        assert_eq!(store_of("syntheses/ab/abcd.md"), Some(SYNTHESES));
        // What is made from a source is not a source, even where a
        // nested store keeps it below its source prefix.
        assert_eq!(store_of("markdown/ab/abcd.md"), None);
        assert_eq!(store_of("personal/markdown/ab/abcd.md"), None);
        assert_eq!(store_of("web/catalog/ab/abcd.yaml"), None);
        assert_eq!(store_of("syntheses/boxes/ab/abcd.words.json.gz"), None);
        // A prefix matches whole path segments only.
        assert_eq!(store_of("papers_old/ab/abcd.pdf"), None);
        assert_eq!(store_of("papers"), None);
        assert_eq!(store_of("elsewhere/ab/abcd.pdf"), None);
    }

    #[test]
    fn the_personal_prefix_is_configurable_and_checked() {
        let stores = Stores::with_personal_prefix("private/docs/").unwrap();
        let (_, layout) = stores.documents(PERSONAL).unwrap();
        assert_eq!(layout.source_prefix, "private/docs");
        assert_eq!(layout.markdown_prefix, "private/docs/markdown");
        assert_eq!(
            stores
                .for_source_key("private/docs/ab/abcd.pdf")
                .map(|(spec, _)| spec.name),
            Some(PERSONAL)
        );

        for bad in ["", "/", "/srv/personal", "a/../b", "a//b", "./a", "a\\b"] {
            assert!(
                matches!(
                    Stores::with_personal_prefix(bad),
                    Err(StoreConfigError::InvalidPrefix { .. })
                ),
                "{bad:?} was accepted"
            );
        }
        // Inside, equal to or around another store's prefix.
        for overlapping in [
            "papers",
            "papers/personal",
            "markdown",
            "web",
            "web/markdown",
        ] {
            let err = Stores::with_personal_prefix(overlapping).unwrap_err();
            assert!(
                matches!(
                    err,
                    StoreConfigError::Overlap {
                        store: PERSONAL,
                        ..
                    }
                ),
                "{overlapping:?}: {err}"
            );
        }
    }

    #[test]
    fn the_personal_prefix_comes_from_the_environment_then_the_user_then_the_system() {
        let dir = tempfile::tempdir().unwrap();
        let absent = ConfigFile::load_at(&dir.path().join("absent.yaml"), dir.path()).unwrap();
        let system = config(dir.path(), "system.yaml", "personal:\n  storage_dir: sys\n");
        let user = config(
            dir.path(),
            "user.yaml",
            "personal:\n  storage_dir: mine/\n  ingest_inbox: in\n",
        );
        // A `personal:` section without the key says nothing about it.
        let silent = config(dir.path(), "silent.yaml", "personal:\n  ingest_inbox: in\n");

        let resolve = |system: &ConfigFile, user: &ConfigFile, env: Option<&str>| {
            resolve_personal_prefix(system, user, env.map(str::to_string)).unwrap()
        };
        assert_eq!(resolve(&absent, &absent, None), "personal");
        assert_eq!(resolve(&system, &absent, None), "sys");
        assert_eq!(resolve(&system, &silent, None), "sys");
        assert_eq!(resolve(&system, &user, None), "mine");
        assert_eq!(resolve(&system, &user, Some("env")), "env");

        // A value that cannot be a prefix is refused wherever it came from.
        assert!(resolve_personal_prefix(&system, &user, Some(String::new())).is_err());
        let wrong = config(dir.path(), "wrong.yaml", "personal:\n  storage_dir: [a]\n");
        assert!(matches!(
            resolve_personal_prefix(&absent, &wrong, None),
            Err(StoreConfigError::Config(_))
        ));
    }
}
