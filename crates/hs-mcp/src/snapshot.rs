//! The dashboard's snapshot: everything `hs status` draws, gathered once.
//!
//! It is served at `GET /mcp/status`, not as a tool: it lists the catalog
//! and the storage, which takes seconds on a large corpus and is a
//! person's dashboard, not something an agent decides by. An agent asks
//! the `status` tool, which reads only sources that answer in constant
//! time.

use crate::HomeStillMcp;

/// Split a service answer into the value and the reason it is missing, so a
/// status tool reports why a service is down instead of a bare `null`.
fn split_result<T, E: std::fmt::Display>(r: Result<T, E>) -> (Option<T>, Option<String>) {
    match r {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    }
}

/// `"health: <why>; readiness: <why>"` for the calls that failed; `None`
/// when none did. Carried into the system snapshot so an unhealthy instance
/// says why.
fn join_call_errors(calls: &[(&str, &Option<String>)]) -> Option<String> {
    let parts: Vec<String> = calls
        .iter()
        .filter_map(|(name, err)| err.as_ref().map(|e| format!("{name}: {e}")))
        .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Why a scribe tier is refusing work, in one line.
///
/// "backend unavailable" is a symptom: it is what a crashed server, a
/// missing model, a wrong URL and a broken converter all look like.
/// The tier's admission gate is a *measurement*, and the measurement is
/// already in the health payload — the free VRAM, the headroom it
/// wants, and who is holding the card. This turns the symptom into a
/// diagnosis, and a diagnosis into an action.
fn backend_unavailable_reason(health: Option<&hs_scribe::client::HealthResponse>) -> String {
    let Some(h) = health else {
        return "backend unavailable".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(holder) = h.backend_yielding_to.as_deref() {
        parts.push(format!("GPU yielded to {holder}"));
    }
    if h.backend_reachable == Some(false) {
        parts.push("the VLM backend is unreachable".to_string());
    }
    if let Some(free) = h.backend_vram_free_mb {
        match h.backend_vram_headroom_mb {
            Some(headroom) if free < headroom => parts.push(format!(
                "{:.1} GiB free, needs {:.1} GiB",
                free as f64 / 1024.0,
                headroom as f64 / 1024.0
            )),
            Some(headroom) => parts.push(format!(
                "{:.1} GiB free, {:.1} GiB needed",
                free as f64 / 1024.0,
                headroom as f64 / 1024.0
            )),
            None => parts.push(format!("{:.1} GiB free", free as f64 / 1024.0)),
        }
    }
    if let Some(holders) = h.backend_vram_holders.as_deref() {
        let card = holders_summary(holders);
        if !card.is_empty() {
            parts.push(format!("card held by {card}"));
        }
    }
    if parts.is_empty() {
        "backend unavailable".to_string()
    } else {
        // "·" separates clauses: the diagnosis is read, not parsed.
        format!("backend unavailable: {}", parts.join(" · "))
    }
}

/// Who is on the card, folded into something a person can read.
///
/// The health payload carries one `pid=… NNNMB name` entry per process,
/// which is `nvidia-smi`'s shape, not a sentence. Three problems with
/// printing it verbatim in a status row: the pid answers no question an
/// operator of *this* tool has (the action is "wait for the tenant to
/// idle", not "kill 220903"), MB is the only MB among GiB neighbours, and
/// the length is what forces the row to wrap mid-token.
///
/// So: same process name folds into one entry, sizes report in GiB, pids
/// drop. Two `llama-server`s are two processes but one tenant — summing
/// them is the honest answer to "how much of my card is llama-server".
fn holders_summary(holders: &str) -> String {
    // Largest first, so the tenant that actually took the card leads.
    let mut by_name: Vec<(String, u64)> = Vec::new();
    for entry in holders.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // `pid=<n> <mb>MB <name>` — keep the name and the size, skip the pid.
        // An entry with no size is not a holder report (a truncated or
        // garbled string), so it is dropped rather than printed raw.
        let mut mb: Option<u64> = None;
        let mut name = String::new();
        for token in entry.split_whitespace() {
            if token.starts_with("pid=") {
                continue;
            }
            if let Some(size) = token.strip_suffix("MB").and_then(|n| n.parse::<u64>().ok()) {
                mb = Some(size);
                continue;
            }
            if name.is_empty() {
                name = token.to_string();
            }
        }
        let (Some(mb), false) = (mb, name.is_empty()) else {
            continue;
        };
        match by_name.iter_mut().find(|(existing, _)| *existing == name) {
            Some((_, total)) => *total += mb,
            None => by_name.push((name, mb)),
        }
    }
    by_name.sort_by_key(|(_, mb)| std::cmp::Reverse(*mb));
    by_name
        .iter()
        .map(|(name, mb)| format!("{name} {:.1} GiB", *mb as f64 / 1024.0))
        .collect::<Vec<_>>()
        .join(", ")
}

impl HomeStillMcp {
    /// Build the StatusSnapshot that `GET /mcp/status` serves and `hs status`
    /// draws. It describes the `academic` store's pipeline.
    pub(crate) async fn build_status_snapshot(
        &self,
        history_limit: usize,
        include_repaired: bool,
    ) -> hs_common::status::StatusSnapshot {
        use hs_common::status::{
            build_history, collect_pipeline_counts, ServiceInstance, StatusSnapshot, StoreInfo,
        };

        // Per-distill health fanout (also yields embedded doc/chunk counts).
        let mut distill_instances: Vec<ServiceInstance> = Vec::new();
        let mut embedded_documents: Option<u64> = None;
        let mut embedded_chunks: Option<u64> = None;
        let mut store_info: Option<StoreInfo> = None;
        for url in &self.distill_servers {
            let client = match hs_distill::client::DistillClient::new(url) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("distill client build for {url} failed: {e}");
                    continue;
                }
            };
            let (health, health_err) = split_result(client.health().await);
            let (readiness, readiness_err) = split_result(client.readiness().await);
            let (status, status_err) =
                split_result(client.status(hs_common::stores::ACADEMIC).await);
            let error = join_call_errors(&[
                ("health", &health_err),
                ("readiness", &readiness_err),
                ("status", &status_err),
            ]);

            let healthy = health.is_some();
            let version = health
                .as_ref()
                .map(|h| h.version.clone())
                .unwrap_or_default();
            let compute_device = status
                .as_ref()
                .map(|s| s.compute_device.clone())
                .or_else(|| health.as_ref().map(|h| h.compute_device.clone()))
                .unwrap_or_default();
            let embed_model = status
                .as_ref()
                .map(|s| s.embed_model.clone())
                .or_else(|| health.as_ref().map(|h| h.embed_model.clone()))
                .unwrap_or_default();
            // The store these numbers are about: the one `status` was asked
            // for. A server that answered neither call reports none.
            let collection = if healthy || status.is_some() {
                hs_common::stores::ACADEMIC.to_string()
            } else {
                String::new()
            };
            let in_flight = readiness.as_ref().map(|r| r.in_flight as u64).unwrap_or(0);
            let activity = if !healthy {
                format!("unhealthy: {}", health_err.as_deref().unwrap_or("unknown"))
            } else if in_flight > 0 {
                format!("{in_flight} embedding")
            } else {
                "idle".to_string()
            };

            if let Some(s) = status.as_ref() {
                if embedded_documents.is_none() {
                    embedded_documents = Some(s.documents_count);
                    embedded_chunks = Some(s.records_count);
                }
            }
            if store_info.is_none() && !collection.is_empty() {
                store_info = Some(StoreInfo {
                    store: collection.clone(),
                    compute_device: compute_device.clone(),
                    embed_model: embed_model.clone(),
                    version: health
                        .as_ref()
                        .map(|h| h.backend_version.clone())
                        .unwrap_or_default(),
                    // Served only by the token-protected /status.
                    url: status
                        .as_ref()
                        .map(|s| s.myelin_url.clone())
                        .unwrap_or_default(),
                });
            }

            distill_instances.push(ServiceInstance {
                url: url.clone(),
                healthy,
                version,
                compute_device,
                embed_model,
                collection,
                activity,
                in_flight,
                slots_total: None,
                slots_available: None,
                error,
            });
        }

        // Per-scribe health fanout.
        let mut scribe_instances: Vec<ServiceInstance> = Vec::new();
        for url in &self.scribe_servers {
            let client = match hs_scribe::client::ScribeClient::new(url) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("scribe client build for {url} failed: {e}");
                    continue;
                }
            };
            let (health, health_err) = split_result(client.health().await);
            let (readiness, readiness_err) = split_result(client.readiness().await);
            let error = join_call_errors(&[("health", &health_err), ("readiness", &readiness_err)]);

            // A scribe whose VLM backend can't take work answers 503
            // with `status: "backend_unavailable"`. The body still
            // parses, so "reachable" is not "usable" — key off status.
            let status = health.as_ref().map(|h| h.status.as_str());
            let backend_unavailable = status == Some(hs_scribe::client::BACKEND_UNAVAILABLE);
            let healthy = status == Some("ok");
            let version = health
                .as_ref()
                .map(|h| h.version.clone())
                .unwrap_or_default();
            let (in_flight, slots_total, slots_available) = match readiness.as_ref() {
                Some(r) => (
                    r.in_flight_conversions as u64,
                    Some(r.vlm_slots_total as u64),
                    Some(r.vlm_slots_available as u64),
                ),
                None => (0, None, None),
            };
            let activity = if backend_unavailable {
                // A diagnosis, not a symptom. "backend unavailable"
                // is indistinguishable from a crashed server, a missing
                // model, a wrong URL or a broken converter — and the
                // operator's next step is to read a journal on another
                // host. The gate's own numbers turn it into an action.
                backend_unavailable_reason(health.as_ref())
            } else if !healthy {
                format!(
                    "unhealthy: {}",
                    health_err.as_deref().unwrap_or("status not ok")
                )
            } else if in_flight > 0 {
                format!("{in_flight} converting")
            } else {
                "idle".to_string()
            };

            scribe_instances.push(ServiceInstance {
                url: url.clone(),
                healthy,
                version,
                compute_device: String::new(),
                embed_model: String::new(),
                collection: String::new(),
                activity,
                in_flight,
                slots_total,
                slots_available,
                error,
            });
        }

        // Scan the catalog once; reuse for both the pipeline failed-count
        // and the history panel so we don't pay to deserialize every YAML twice.
        let (catalog_triples, catalog_error) =
            match hs_common::catalog::list_catalog_entries_parallel(
                &*self.storage,
                &self.academic.catalog_prefix,
                hs_common::catalog::CATALOG_FETCH_CONCURRENCY,
                |_, _| {},
            )
            .await
            {
                Ok(triples) => (Some(triples), None),
                Err(e) => (None, Some(format!("listing the catalog failed: {e:#}"))),
            };

        // Count entries stamped `embedding_skip` so progress percentages
        // can exclude intentionally-skipped docs from the denominator.
        let embedding_skipped: Option<u64> = catalog_triples.as_ref().map(|triples| {
            triples
                .iter()
                .filter(|(_, _, entry)| entry.embedding_skip.is_some())
                .count() as u64
        });

        // Same pass — catalog rows stamped `conversion_failed`, by what a
        // retry can do (`hs status` renders them as the Failed row).
        let failed_conversions = catalog_triples.as_ref().map(|triples| {
            hs_common::status::FailedConversions::tally(
                triples
                    .iter()
                    .filter_map(|(_, _, entry)| entry.conversion_failed.as_ref())
                    .map(|f| f.reason.as_str()),
                hs_scribe::classify::stamp_kind,
            )
        });

        // Inbox queue — files waiting in `papers/manually_downloaded/` for
        // the next sweep tick. Filtered by the same whitelist the sweeper
        // applies, so the dashboard number tracks what the daemon will
        // actually relocate. A listing failure is logged with its cause and
        // leaves the count unknown (`Inbox ···`) rather than failing the
        // whole snapshot.
        let inbox_prefix = format!(
            "{}/manually_downloaded/",
            self.academic.source_prefix.trim_end_matches('/')
        );
        let inbox_pending: Option<u64> = match self.storage.list(&inbox_prefix).await {
            Ok(objs) => Some(
                objs.iter()
                    .filter(|o| {
                        let fn_ = o.key.rsplit('/').next().unwrap_or("");
                        hs_common::inbox::is_inbox_candidate_filename(fn_)
                    })
                    .count() as u64,
            ),
            Err(e) => {
                tracing::warn!(prefix = %inbox_prefix, error = %e, "inbox listing failed; the inbox count is unknown");
                None
            }
        };

        // A failed listing must not read as a fully converted, empty corpus:
        // mark the counts unavailable so `hs status` renders "unknown".
        let mut pipeline = match collect_pipeline_counts(
            &*self.storage,
            &self.academic.source_prefix,
            &self.academic.markdown_prefix,
            &self.academic.catalog_prefix,
            embedded_documents,
            embedded_chunks,
            embedding_skipped,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                let mut p = hs_common::status::PipelineCounts {
                    embedded_documents,
                    embedded_chunks,
                    embedding_skipped,
                    ..Default::default()
                };
                p.mark_unavailable(&e);
                p
            }
        };
        if let Some(reason) = &catalog_error {
            pipeline.mark_unavailable(reason);
        }

        // Pipeline drift: distinct source stems that haven't produced
        // markdown yet, less whatever is converting right now. Saturating
        // subtraction so stage lag never yields a negative.
        //
        // Counted per *stem*, not per object: a paper stored as both `.pdf`
        // and `.html` yields one markdown, and markdown whose source was
        // removed by the DOI-stem lowercasing migration has no source at
        // all. The old `documents - markdown` object arithmetic charged the
        // former as backlog and credited the latter against it, holding the
        // metric at 71 against a threshold of 3 no matter how much the
        // pipeline converted. See `count_unconverted_stems`.
        //
        // By design, catalog rows stamped `conversion_failed` that still
        // sit in the live papers tree (surfaced separately as
        // `failed_conversions`) are NOT subtracted — drift is meant to surface
        // them too, since failed converts represent stuck pipeline state
        // the operator should see. Values above `pipeline_drift_threshold`
        // indicate either stamped failures or stems that errored without a
        // stamp; check scribe/event-watch logs for the latter.
        //
        // Stalled, unlike drift, leaves stamped stems out: it is what `hs
        // pipeline catch-up` would queue, and catch-up skips them. Without
        // the catalog the stamped stems are unknown, and so is Stalled.
        let total_in_flight: u64 = scribe_instances.iter().map(|s| s.in_flight).sum();
        let stamped: Option<std::collections::HashSet<&str>> =
            catalog_triples.as_ref().map(|triples| {
                triples
                    .iter()
                    .filter(|(_, _, entry)| entry.conversion_failed.is_some())
                    .map(|(stem, _, _)| stem.as_str())
                    .collect()
            });
        let unstamped_unconverted = match hs_common::status::count_unconverted_stems(
            &*self.storage,
            &self.academic.source_prefix,
            &self.academic.markdown_prefix,
            stamped.as_ref().unwrap_or(&Default::default()),
        )
        .await
        {
            Ok(unconverted) => {
                pipeline.pipeline_drift = unconverted.all.saturating_sub(total_in_flight);
                stamped.as_ref().map(|_| unconverted.unstamped)
            }
            Err(e) => {
                pipeline.mark_unavailable(&e);
                None
            }
        };
        // Bounded: an unreachable broker must not stall the whole snapshot.
        let depth = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.events
                .queue_depth(&hs_common::event_bus::specs::PAPERS_INGESTED),
        )
        .await
        {
            Ok(Ok(depth)) => Ok(depth.outstanding()),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err("timed out asking the broker for queue depth".to_string()),
        };
        pipeline.record_queue(unstamped_unconverted, depth);
        pipeline.pipeline_drift_threshold = hs_common::status::PIPELINE_DRIFT_THRESHOLD;
        pipeline.failed_conversions = failed_conversions;
        pipeline.inbox_pending = inbox_pending;
        pipeline.in_flight_conversions = Some(total_in_flight);

        // History from the catalog — same source and same default filter as
        // `catalog_recent` so the two activity feeds can never disagree.
        let history = match catalog_triples {
            Some(triples) => {
                let pairs: Vec<(String, hs_common::catalog::CatalogEntry)> =
                    triples.into_iter().map(|(s, _m, e)| (s, e)).collect();
                build_history(&pairs, history_limit, include_repaired)
            }
            None => Vec::new(),
        };

        // Inbox-sweeper heartbeat. Populated server-side so every client
        // (mac_air, big, laptop) renders the same verdict. `None` = no
        // heartbeat key in storage; `Some(.running=false)` = stale.
        // A storage/parse error is surfaced via `counts_error`, not folded
        // into `None` (which means "no heartbeat key").
        let inbox_heartbeat = match hs_common::status::read_inbox_heartbeat(&*self.storage).await {
            Ok(hb) => hb,
            Err(e) => {
                pipeline.mark_unavailable(&e);
                None
            }
        };

        StatusSnapshot {
            pipeline,
            scribe_instances,
            distill_instances,
            store: store_info,
            history,
            inbox_heartbeat,
            generated_at: Some(chrono::Utc::now().to_rfc3339()),
        }
    }
}

#[cfg(test)]
mod backend_reason_tests {
    use super::*;

    fn health(
        free: Option<u64>,
        headroom: Option<u64>,
        holders: Option<&str>,
        resident: Option<bool>,
    ) -> hs_scribe::client::HealthResponse {
        hs_scribe::client::HealthResponse {
            status: "backend_unavailable".into(),
            layout_model: false,
            table_model: false,
            layout_model_reason: None,
            table_model_reason: None,
            version: "0.0.1-rc.374".into(),
            gpu_name: None,
            gpu_utilization_pct: None,
            gpu_memory_used_mb: None,
            last_conversion_at: None,
            total_conversions: 0,
            backend_reachable: Some(true),
            backend_model_resident: resident,
            backend_vram_free_mb: free,
            backend_vram_headroom_mb: headroom,
            backend_vram_holders: holders.map(str::to_string),
            backend_yielding_to: None,
            backend_checked_at: None,
        }
    }

    /// The report must name the shortfall: a free-VRAM number on its
    /// own says nothing, and "backend unavailable" says even less.
    #[test]
    fn a_shortfall_is_reported_with_both_numbers() {
        let h = health(Some(13251), Some(13500), None, Some(false));
        let reason = backend_unavailable_reason(Some(&h));
        assert!(reason.contains("12.9 GiB free"), "{reason}");
        assert!(reason.contains("13.2 GiB"), "{reason}");
    }

    /// The holders are the question an operator actually has: who is
    /// holding the card. The answer names the tenants in GiB, not raw
    /// pids and MB — a pid is not actionable from a status row.
    #[test]
    fn the_holders_are_named_in_gib_without_pids() {
        let h = health(
            Some(13251),
            Some(13500),
            Some("pid=2227527 7534MB hs-distill-server; pid=1737565 2596MB hs-scribe-server"),
            Some(false),
        );
        let reason = backend_unavailable_reason(Some(&h));
        assert!(reason.contains("hs-distill-server 7.4 GiB"), "{reason}");
        assert!(reason.contains("hs-scribe-server 2.5 GiB"), "{reason}");
        assert!(!reason.contains("pid="), "a pid is noise here: {reason}");
        assert!(!reason.contains("MB"), "sizes are GiB beside GiB: {reason}");
    }

    /// LARGEST FIRST, so the tenant that actually took the card leads.
    #[test]
    fn the_biggest_tenant_leads() {
        let h = health(
            Some(1051),
            Some(13500),
            Some("pid=1 500MB ollama; pid=2 15960MB llama-server"),
            Some(false),
        );
        let reason = backend_unavailable_reason(Some(&h));
        let llama = reason.find("llama-server").expect("llama-server named");
        let ollama = reason.find("ollama").expect("ollama named");
        assert!(llama < ollama, "the card's owner leads: {reason}");
    }

    /// Two processes, one tenant: summing them answers "how much of my
    /// card is llama-server", which is the question worth answering.
    #[test]
    fn a_tenant_running_two_processes_is_folded_into_one_entry() {
        let h = health(
            Some(1051),
            Some(13500),
            Some("pid=1 15960MB llama-server; pid=2 920MB llama-server; pid=3 500MB ollama"),
            Some(false),
        );
        let reason = backend_unavailable_reason(Some(&h));
        assert_eq!(
            reason.matches("llama-server").count(),
            1,
            "one entry per tenant: {reason}"
        );
        assert!(reason.contains("llama-server 16.5 GiB"), "{reason}");
    }

    /// A name that is not the last token on the line is still read whole,
    /// and a malformed entry is dropped rather than printed raw.
    #[test]
    fn malformed_holder_entries_are_dropped_not_printed() {
        assert_eq!(holders_summary(""), "");
        assert_eq!(holders_summary("pid=1 500MB"), "");
        assert_eq!(holders_summary("garbage; ; pid=2 100MB ok"), "ok 0.1 GiB");
    }

    /// A model that is already resident needs no room to start, so no
    /// headroom is reported against it.
    #[test]
    fn a_resident_model_reports_no_headroom_requirement() {
        let h = health(Some(6000), None, None, Some(true));
        let reason = backend_unavailable_reason(Some(&h));
        assert!(!reason.contains("needs"), "{reason}");
        assert!(reason.contains("5.9 GiB free"), "{reason}");
    }

    /// A health body with no numbers at all still says something true.
    #[test]
    fn a_body_with_no_numbers_falls_back_to_the_symptom() {
        let h = health(None, None, None, None);
        assert_eq!(backend_unavailable_reason(Some(&h)), "backend unavailable");
        assert_eq!(backend_unavailable_reason(None), "backend unavailable");
    }
}
