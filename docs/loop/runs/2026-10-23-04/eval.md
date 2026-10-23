# Eval — `hs upgrade` leaves an unchanged, healthy compose service running (rc.388)

## Before (measured)

The exact rc.387 upgrade sequence (`pull` → `down` → `up -d`, then wait for
`/healthz`) on a throwaway Qdrant: a reflink copy of the live 7.3 GB
storage, same image, port 16333. The image was already current, so the
pull changed nothing.

| run | pull | down | up | until healthy | search outage |
|---|---|---|---|---|---|
| 1 | 1.0 s | 1.4 s | 1.5 s | 41.9 s | **44.8 s** |
| 2 | 4.0 s | 3.9 s | 3.6 s | 59.5 s | **67.0 s** |

Production, 2026-10-22: ~7 minutes, and the upgrade then failed
`Timed out waiting for Qdrant … (90s)` on a healthy host (BACKLOG P0-25).

## After

The decision the new code makes on big's live Qdrant, probed read-only
with the same commands it runs: the container (`compose ps -q`) runs image
`8a80c550…`; the `latest` tag (`image inspect`) names `8a80c550…`;
`/healthz` answers 200 → **left running**: no `down`, no `up`, no outage,
and the post-upgrade wait passes on its first poll.

A changed image, a missing container, an unhealthy service or any probe
failing → recreate, exactly as before.

## Tests

| test | guards |
|---|---|
| `containers_are_unchanged_only_when_they_run_exactly_the_named_images` | equal ids (with/without `sha256:`) leave running; moved tag, missing container, service/container count mismatch, empty project recreate |
| `service_images_come_from_the_resolved_config_and_require_an_image` | parses real `podman-compose config` output; a `build:`-only service is an error (→ recreate) |
| `an_unchanged_healthy_service_is_left_running` | stand-in runtime + engine + health server: `pull`, `config`, `ps` — never `down`/`up` |
| `a_moved_tag_or_an_unhealthy_service_is_recreated` | both cases run `down` and `up` |
| existing `compose_*` tests | `pull`/`down`/`up` failures still fail the upgrade |

**Not done (time):** an end-to-end run of the real function against the
throwaway Qdrant. The live read-only probe above shows the inputs the
function sees on big; the stand-in tests show what it does with them.

## Unchanged

Search, scribe, distill: untouched. The 90 s post-upgrade wait still
applies when an image really changed (P0-25's other half stays open).

# Perf

| | rc.387 | rc.388 |
|---|---|---|
| Qdrant outage per `hs upgrade`, image unchanged | 44.8–67.0 s (measured), ~7 min (production) | **0 s** |
| extra commands per compose file | — | `compose config`, `compose ps -q`, one `image inspect` per service, one `inspect` per container, one 5 s-bounded GET |
