pub mod duckdb_loader;
pub mod lookup;
pub mod model;
pub mod parser;
pub mod reader;
pub mod schema;
pub mod seen_set;

pub use duckdb_loader::{EntityStats, OpenAlexDb, SimpleEntity};
pub use lookup::{lookup_work_abstract_by_doi, WorkAbstract};
pub use model::{
    Author, Concept, Domain, Field, Funder, Institution, Publisher, Source, Subfield, Topic, Work,
};
pub use parser::{parse_work_id_u64, reconstruct_abstract, strip_doi, strip_openalex_id};
pub use reader::{list_partitions, read_jsonl_file, read_partition};
pub use seen_set::SeenSet;
