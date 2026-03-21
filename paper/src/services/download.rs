use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::stream::{self, StreamExt};

use crate::error::PaperError;
use crate::models::{BatchDownloadResult, DownloadFailure, DownloadResult, Paper};
use crate::ports::download_service::DownloadService;

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
}

pub type OnProgress = Arc<dyn Fn(DownloadEvent) + Send + Sync>;

pub async fn download_batch(
    service: Arc<dyn DownloadService>,
    papers: Vec<Paper>,
    max_concurrent: usize,
    on_progress: Option<OnProgress>,
) -> BatchDownloadResult {
    let total = papers.len();
    let completed = Arc::new(AtomicUsize::new(0));

    let results: Vec<Result<DownloadResult, (Paper, String)>> = stream::iter(papers)
        .map(|paper| {
            let service = Arc::clone(&service);
            let completed = Arc::clone(&completed);
            let on_progress = on_progress.clone();

            async move {
                let title = paper.title.clone();

                if let Some(ref cb) = on_progress {
                    let count = completed.load(Ordering::Relaxed);
                    cb(DownloadEvent::Started {
                        index: count,
                        total,
                        title: title.clone(),
                    })
                }

                let result = download_single(
                    service.as_ref(),
                    &paper,
                    on_progress.as_ref(),
                    completed.load(Ordering::Relaxed),
                )
                .await;

                let count = completed.fetch_add(1, Ordering::Relaxed) + 1;

                match &result {
                    Ok(dr) => {
                        if let Some(ref cb) = on_progress {
                            cb(DownloadEvent::Completed {
                                index: count,
                                total,
                                size_bytes: dr.size_bytes,
                            })
                        }
                    }
                    Err((_, err)) => {
                        if let Some(ref cb) = on_progress {
                            cb(DownloadEvent::Failed {
                                index: count,
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
    let mut failed = Vec::new();

    for result in results {
        match result {
            Ok(dr) => succeeded.push(dr),
            Err((paper, error)) => failed.push(DownloadFailure {
                paper_id: paper.id,
                title: paper.title,
                error,
            }),
        }
    }

    BatchDownloadResult {
        succeeded,
        failed,
        total_requested: total,
    }
}

#[allow(clippy::type_complexity)]
async fn download_single(
    service: &dyn DownloadService,
    paper: &Paper,
    on_progress: Option<&OnProgress>,
    index: usize,
) -> Result<DownloadResult, (Paper, String)> {
    let filename = format!("{}.pdf", sanitize_filename(&paper.id));
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

    let result = if let Some(ref url) = paper.download_url {
        service.download_by_url(url, &filename, progress_ref).await
    } else if let Some(ref doi) = paper.doi {
        service.download_by_doi(doi).await
    } else {
        Err(PaperError::NoDownloadUrl(paper.id.clone()))
    };

    match result {
        Ok(mut dr) => {
            dr.doi = paper.doi.clone();
            Ok(dr)
        }
        Err(e) => Err((paper.clone(), e.to_string())),
    }
}

fn sanitize_filename(id: &str) -> String {
    id.replace(['/', '\\', ':'], "_")
}
