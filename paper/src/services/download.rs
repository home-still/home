use std::sync::Arc;

use futures::stream::{self, StreamExt};

use crate::error::PaperError;
use crate::models::{BatchDownloadResult, DownloadFailure, DownloadResult, Paper};
use crate::ports::download_service::DownloadService;
use crate::providers::url_guard;
use crate::stem;

pub enum DownloadEvent {
    Started {
        index: usize,
        total: usize,
        title: String,
    },
    Progress {
        index: usize,
        bytes_downloaded: u64,
        bytes_total: Option<u64>,
        title: String,
    },
    Completed {
        index: usize,
        total: usize,
        size_bytes: u64,
    },
    Failed {
        index: usize,
        total: usize,
        title: String,
        error: String,
    },
    Skipped {
        index: usize,
        total: usize,
        size_bytes: u64,
    },
}

pub type OnProgress = Arc<dyn Fn(DownloadEvent) + Send + Sync>;

/// Download `papers` with at most `max_concurrent` in flight. Rejects
/// `max_concurrent == 0`: `buffer_unordered(0)` never polls anything, so the
/// batch would hang forever instead of failing.
pub async fn download_batch(
    service: Arc<dyn DownloadService>,
    papers: Vec<Paper>,
    max_concurrent: usize,
    on_progress: Option<OnProgress>,
) -> Result<BatchDownloadResult, PaperError> {
    if max_concurrent == 0 {
        return Err(PaperError::InvalidInput(
            "concurrency must be at least 1".to_string(),
        ));
    }
    let total = papers.len();

    let results: Vec<Result<DownloadResult, Box<(Paper, String)>>> =
        stream::iter(papers.into_iter().enumerate())
            .map(|(i, paper)| {
                let service = Arc::clone(&service);
                let on_progress = on_progress.clone();

                async move {
                    let title = paper.title.clone();

                    if let Some(ref cb) = on_progress {
                        cb(DownloadEvent::Started {
                            index: i,
                            total,
                            title: title.clone(),
                        })
                    }

                    let result =
                        download_single(service.as_ref(), &paper, on_progress.as_ref(), i).await;

                    match &result {
                        Ok(dr) => {
                            if let Some(ref cb) = on_progress {
                                if dr.skipped {
                                    cb(DownloadEvent::Skipped {
                                        index: i,
                                        total,
                                        size_bytes: dr.size_bytes,
                                    })
                                } else {
                                    cb(DownloadEvent::Completed {
                                        index: i,
                                        total,
                                        size_bytes: dr.size_bytes,
                                    })
                                }
                            }
                        }
                        Err(boxed) => {
                            let err = &boxed.1;
                            if let Some(ref cb) = on_progress {
                                cb(DownloadEvent::Failed {
                                    index: i,
                                    total,
                                    title: title.clone(),
                                    error: String::from(err),
                                })
                            }
                        }
                    }

                    result
                }
            })
            .buffer_unordered(max_concurrent)
            .collect()
            .await;

    let mut succeeded = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for result in results {
        match result {
            Ok(dr) if dr.skipped => skipped.push(dr),
            Ok(dr) => succeeded.push(dr),
            Err(boxed) => {
                let (paper, error) = *boxed;
                failed.push(DownloadFailure {
                    paper_id: paper.id,
                    title: paper.title,
                    error,
                })
            }
        }
    }

    Ok(BatchDownloadResult {
        succeeded,
        failed,
        total_requested: total,
        skipped,
    })
}

#[allow(clippy::type_complexity)]
// `Paper` is large enough that an unboxed `(Paper, String)` error variant
// bloats every `Result` on the happy path, so the failure payload is boxed.
async fn download_single(
    service: &dyn DownloadService,
    paper: &Paper,
    on_progress: Option<&OnProgress>,
    index: usize,
) -> Result<DownloadResult, Box<(Paper, String)>> {
    let fail = |message: String| Box::new((paper.clone(), message));

    // One storage identity per paper, shared with `download_by_doi`: a paper
    // found by search and the same paper requested by DOI must land on one key.
    let stem = stem::paper_stem(paper).map_err(|e| fail(e.to_string()))?;
    let title = paper.title.clone();

    let chunk_cb: Option<Box<dyn Fn(u64, Option<u64>) + Send + Sync>> = on_progress.map(|cb| {
        let cb = Arc::clone(cb);
        let title = title.clone();
        Box::new(move |bytes_downloaded: u64, bytes_total: Option<u64>| {
            cb(DownloadEvent::Progress {
                index,
                bytes_downloaded,
                bytes_total,
                title: title.clone(),
            });
        }) as Box<dyn Fn(u64, Option<u64>) + Send + Sync>
    });

    let progress_ref = chunk_cb.as_deref();

    // Remote failures are collected so the final message names every
    // attempt; a local failure (storage, invalid key) ends the paper at once
    // with the real error — the next URL cannot fix a disk.
    let mut attempts: Vec<String> = Vec::new();

    for url in &paper.download_urls {
        match service.download_by_url(url, &stem, progress_ref).await {
            Ok(mut dr) => {
                dr.doi = paper.doi.clone();
                return Ok(dr);
            }
            Err(e) if e.is_local() => return Err(fail(e.to_string())),
            Err(e) => attempts.push(format!("{}: {e}", url_guard::display_url(url))),
        }
    }

    if let Some(doi) = &paper.doi {
        match service.download_by_doi(doi).await {
            Ok(mut dr) => {
                dr.doi = paper.doi.clone();
                return Ok(dr);
            }
            Err(e) if e.is_local() => return Err(fail(e.to_string())),
            Err(e) => attempts.push(format!("DOI {doi}: {e}")),
        }
    }

    Err(fail(if attempts.is_empty() {
        format!("No download URL or DOI for paper {}", paper.id)
    } else {
        attempts.join("\n")
    }))
}
