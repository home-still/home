use crate::aggregation::RankedPaper;

const MIN_TITLE_LEN: usize = 10;

pub fn filter_quality(papers: Vec<RankedPaper>) -> Vec<RankedPaper> {
    papers
        .into_iter()
        .filter(|rp| {
            let title = rp.paper.title.trim();
            !title.is_empty() && title.len() > MIN_TITLE_LEN
        })
        .collect()
}
