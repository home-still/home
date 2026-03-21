use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, DebouncedEvent};

use crate::config::AppConfig;
use crate::pipeline::processor::Processor;

pub async fn watch_directory(dir: &Path, config: AppConfig) -> Result<()> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    ctrlc::set_handler(move || {
        shutdown_clone.store(true, Ordering::Relaxed);
    })?;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(500), None, tx)?;
    debouncer
        .watch(dir, RecursiveMode::Recursive)?;

    let processor = Processor::new(config)?;

    tracing::info!("Watching {} for PDF files...", dir.display());

    while !shutdown.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                for event in events {
                    process_event(&event, &processor).await;
                }
            }
            Ok(Err(errors)) => {
                for e in errors {
                    tracing::warn!("Watch error: {e}");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    tracing::info!("Shutting down watcher.");
    Ok(())
}

async fn process_event(event: &DebouncedEvent, processor: &Processor) {
    for path in &event.paths {
        if path.extension().is_some_and(|ext| ext == "pdf") {
            tracing::info!("Detected: {}", path.display());
            let output = path.with_extension("md");
            match processor.process_pdf(path.to_str().unwrap_or_default()).await {
                Ok(markdown) => {
                    if let Err(e) = std::fs::write(&output, &markdown) {
                        tracing::error!("Failed to write {}: {e}", output.display());
                    } else {
                        tracing::info!("Converted -> {}", output.display());
                    }
                }
                Err(e) => tracing::error!("Failed to process {}: {e}", path.display()),
            }
        }
    }
}
