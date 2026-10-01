pub mod abstracts;
pub mod adaptive_batch;
pub mod api;
pub mod chunker;
pub mod cli;
pub mod client;
pub mod collection;
pub mod config;
pub mod embed;
pub mod error;
pub mod event_watch;
pub mod metadata;
pub mod pipeline;
pub mod quality;
pub mod reconcile;
pub mod store;
pub mod text;
pub mod types;

#[cfg(test)]
pub(crate) mod testutil;

#[cfg(feature = "server")]
pub mod qdrant;
#[cfg(feature = "server")]
pub mod server;
