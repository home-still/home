pub mod config;
pub mod paper;

use std::sync::Arc;

use anyhow::Result;
use hs_common::global_args::GlobalArgs;
use hs_common::mode::OutputMode;
use hs_common::reporter::Reporter;
use hs_common::styles::Styles;

use crate::cli::PaperCmd;

pub async fn dispatch(
    cmd: PaperCmd,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
    styles: &Styles,
    mode: &OutputMode,
) -> Result<()> {
    match cmd {
        PaperCmd::Search {
            query,
            search_type,
            show_abstract,
            date,
            max_results,
            offset,
            provider,
            sort_by,
            min_citations,
        } => {
            paper::run_search(
                query,
                date,
                search_type,
                sort_by,
                max_results,
                offset,
                provider,
                show_abstract,
                min_citations,
                global,
                reporter,
                styles,
                mode,
            )
            .await
        }
        PaperCmd::Get { doi, provider } => {
            paper::run_get(doi, provider, global, reporter, styles).await
        }
        PaperCmd::Download {
            query,
            date,
            doi,
            max_results,
            concurrency,
            search_type,
            provider,
        } => {
            paper::run_download(
                query,
                date,
                doi,
                max_results,
                concurrency,
                search_type,
                provider,
                global,
                reporter,
            )
            .await
        }
        PaperCmd::Config { action } => config::run(action, global).await,
    }
}
