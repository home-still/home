# Plan — `hs upgrade` recreates a compose service only when it changed

## Design

`crates/hs/src/upgrade_cmd.rs`, `upgrade_compose_service`:

1. `pull` (unchanged; a failure fails the upgrade).
2. Decide `Recreate` vs `LeaveRunning`:
   - images the service names: `compose -f <file> config`, parsed as YAML
     (`services.*.image`), so interpolation is resolved by the runtime;
   - what each names now: `<engine> image inspect --format {{.Id}} <image>`;
   - what runs: `compose -f <file> ps -q`, then `<engine> inspect
     --format {{.Image}} <id>` per container;
   - `LeaveRunning` iff there is one running container per service, each
     runs an image some service names *now*, and the service's health URL
     answers 200 within a few seconds. Anything else — including any probe
     failing — is `Recreate`: the old, known-good path.
3. `Recreate` → `down` → `up -d` (unchanged). `LeaveRunning` → report
   "<name>: images unchanged and healthy; containers left running".

`<engine>` is the container engine behind the detected compose command:
`docker` for `docker compose`/`docker-compose`, `podman` for `podman
compose`/`podman-compose` — `ComposeCmd::engine()` in `hs-common`.

The decision is a pure function over the probed facts
(`decide_recreate(images_now, running, healthy)`), so it is tested
without a container engine; the probes are thin wrappers.

The health URL per compose file is the one the post-upgrade check
already uses (scribe `/health`, Qdrant `/healthz`); factored into one
function so both use the same URL.

## Not changed

Phase 7's 90 s wait stays; it now runs against a Qdrant that was never
stopped in the common case. A changed image still recreates and can
still outlast it — P0-25's remaining half (wait while the container is
running and recovering) stays open.

## Tests

- `decide_recreate`: unchanged + healthy → leave; image changed →
  recreate; container missing → recreate; unhealthy → recreate;
  `sha256:` prefixes compare equal.
- compose `config` image extraction from real `podman-compose config`
  output.
- Smoke (throwaway, not committed): the real function against the
  throwaway Qdrant project — unchanged → left running, 0 s outage;
  after retagging so the container is stale → recreated.

## Risk

A probe that misreports could leave a stale container running. Every
probe failure maps to `Recreate`, so the failure mode is the old
behaviour, never a skipped upgrade.
