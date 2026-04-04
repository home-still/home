pub mod cli;
pub mod client;
pub mod config;
pub mod error;
pub mod types;

#[cfg(feature = "server")]
pub mod chunker;
#[cfg(feature = "server")]
pub mod embed;
#[cfg(feature = "server")]
pub mod metadata;
#[cfg(feature = "server")]
pub mod pipeline;
#[cfg(feature = "server")]
pub mod qdrant;
#[cfg(feature = "server")]
pub mod server;
