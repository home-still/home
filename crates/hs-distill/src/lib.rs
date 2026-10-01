pub mod abstracts;
pub mod chunker;
pub mod cli;
pub mod client;
pub mod config;
pub mod error;
pub mod event_watch;
pub mod metadata;
pub mod quality;
pub mod reconcile;
pub mod text;
pub mod types;

#[cfg(test)]
pub(crate) mod testutil;

#[cfg(feature = "server")]
pub mod adaptive_batch;
#[cfg(feature = "server")]
pub mod embed;
#[cfg(feature = "server")]
pub mod pipeline;
#[cfg(feature = "server")]
pub mod qdrant;
#[cfg(feature = "server")]
pub mod server;
