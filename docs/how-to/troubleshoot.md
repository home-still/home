# Troubleshoot home-still

This page maps what you see to what is wrong and what fixes it. Find your
symptom, read its cause, and follow its steps.

## Before you start

- Start with the dashboard. `hs status` shows which service or pipeline row is
  off, and its rows are described in the
  [status dashboard reference](../reference/status-dashboard.md).
- Read a service's log with `journalctl -u <unit> -n 100`. The unit names are in
  [Services, ports and endpoints](../reference/services-and-ports.md#service-units);
  user units take `--user`.
- Every service stops with an error that names the cause. Read the last lines
  of the log before you change anything.

## A scribe or distill server does not start

**Symptom.** The unit restarts in a loop, or `/health` never answers. The log
ends with one of:

- `myelin is not usable at <url>: …` from the distill server;
- `myelin at <url> refused the store registry: …` from the distill server;
- `cannot verify where bge-m3-8k:latest runs: …` from the distill server;
- `ollama runs bge-m3-8k:latest on `cpu`, not fully on the GPU. …` from the
  distill server (the device can also be `partial`);
- `bge-m3-8k:latest cannot embed a long chunk: …` from the distill server;
- `the execution provider could not be registered` from the scribe server;
- `libpdfium could not be loaded (searched …)` from the scribe server;
- an error that names `HS_BACKEND_TOKEN`.

**Cause.** The distill server does no embedding itself. It writes to a myelin
server, which asks Ollama for `bge-m3-8k:latest` vectors, and it refuses to start,
and reports itself not ready, unless Ollama holds that model entirely on the
GPU. It never accepts the CPU. The scribe server runs its layout and table
models on CUDA on Linux and does not fall back either. The usual reasons:

1. **myelin does not answer.** The distill server connects to
   `distill_server.myelin_url` (default `http://127.0.0.1:7462/mcp`) before it
   listens. The service is stopped, listens on another address, or is the
   agent surface (port 7463), which refuses the writes the distill server
   needs. `myelin-mcp` has no `/health` route; its own service log says why it
   is down, for example that its Qdrant or Ollama is not reachable.
2. **Ollama cannot be reached or has no `bge-m3-8k`.** The distill server asks
   `distill_server.ollama_url` where the model is loaded, and loads it with one
   embedding request when it is not. An unreachable Ollama, or one that does not
   have the model, stops the start. So does a `bge-m3-8k` that was made
   without `num_batch 8192`: the server sends one long input at start-up and
   stops when Ollama reads only part of it.
3. **Ollama holds the model on the CPU, or split between CPU and GPU.** The
   card is full (another program holds its memory) or Ollama's CUDA is broken.
4. **libpdfium is missing.** The scribe server binds it from `~/.local/lib`,
   `~/.home-still/dyld-libs` or the system library path, and stops when it
   cannot.
5. **The scribe server's ONNX Runtime CUDA provider cannot load its
   libraries.** The provider is built against CUDA 12 (`libcublas.so.12`,
   `libcudart.so.12`, `libcufft.so.11`), and a host that has only CUDA 13 does
   not have them.
6. **The shared secret is missing.** The servers refuse to start without
   `HS_BACKEND_TOKEN`.

**Fix.**

1. Check where Ollama holds the embedding model, and what holds the GPU:

   ```sh
   ollama ps
   nvidia-smi --query-compute-apps=pid,used_gpu_memory,process_name --format=csv
   ```

   `ollama ps` must list `bge-m3-8k:latest` with `100% GPU`. When it says `CPU`
   or a split, stop or move the program that holds the card's memory, or fix
   Ollama's CUDA, then unload the model (`ollama stop bge-m3-8k:latest`) so the
   next request loads it again. When the model is missing, or the log says it
   cannot embed a long chunk, make it as in
   [set up myelin](../tutorials/home-cloud-cluster.md#8-set-up-myelin-on-the-gpu-host).

2. Check myelin and the settings that point at it:

   ```sh
   systemctl --user status myelin-mcp
   journalctl --user -u myelin-mcp -n 100
   hs config show
   ```

   `distill_server.myelin_url` must name myelin's full surface, and
   `distill_server.ollama_url` the Ollama that myelin embeds with. How myelin
   is installed is in
   [set up myelin](../tutorials/home-cloud-cluster.md#8-set-up-myelin-on-the-gpu-host).

3. For the scribe server, check which libraries the provider cannot find:

   ```sh
   ldd ~/.cache/ort.pyke.io/dfbin/*/*/libonnxruntime_providers_cuda.so | grep "not found"
   ```

   If it prints `libcublas.so.12`, `libcudart.so.12` or `libcufft.so.11`, put the
   CUDA 12 runtime libraries in `~/.home-still/cuda12-libs/`. `hs serve scribe`
   adds that directory to the server's library path when it starts:

   ```sh
   mkdir -p ~/.home-still/cuda12-libs
   cd ~/.home-still/cuda12-libs
   python3 <<'PY'
   import urllib.request, json, os, zipfile
   for pkg in ['nvidia-cuda-runtime-cu12', 'nvidia-cublas-cu12', 'nvidia-cufft-cu12']:
       data = json.loads(urllib.request.urlopen(f'https://pypi.org/pypi/{pkg}/json').read())
       wheel = next(u for u in data['urls']
                    if u['filename'].endswith('_x86_64.whl') and 'linux' in u['filename'])
       urllib.request.urlretrieve(wheel['url'], wheel['filename'])
       with zipfile.ZipFile(wheel['filename']) as z:
           for n in z.namelist():
               base = os.path.basename(n)
               if ('.so' in n) and not base.startswith(('libnvrtc-builtins', 'libnvJitLink')):
                   open(base, 'wb').write(z.read(n))
       os.remove(wheel['filename'])
   PY
   ```

   Run the `ldd` line again: nothing reads `not found` any more.

4. If CUDA lives outside `/opt/cuda` and `/usr/local/lib`, add its library
   directory to the scribe unit's `LD_LIBRARY_PATH`:

   ```sh
   sudo systemctl edit hs-serve-scribe
   ```

   In the editor, add `[Service]` and `Environment=LD_LIBRARY_PATH=<cuda-lib-dir>`,
   then save.

5. If the scribe server cannot find pdfium, install it as in
   [step 7 of the cluster tutorial](../tutorials/home-cloud-cluster.md#7-set-up-conversion-on-the-gpu-host).
   `ldconfig -p | grep pdfium` lists a system-wide copy.

6. If the log names `HS_BACKEND_TOKEN`, add the shared secret to
   `~/.home-still/secrets.env` (at least 32 visible characters, `chmod 600`) and
   restart the unit.

**Check it worked.** `curl -fsS http://localhost:7434/health` and
`curl -fsS http://localhost:7433/health` answer with HTTP 200, and the distill
server's log says that myelin is reachable and that the embedding model is on
the GPU.

## The distill server answers 503

**Symptom.** The distill server runs, but `/health` or `/readiness` answers
503, and the `reason` (or the error) is `embedder unavailable`, `embedder not
on GPU`, `myelin unavailable`, or a check that timed out. An index request can
fail with the first two.

**Cause.** The server checks its dependencies on every health probe and before
every index. `embedder unavailable` means Ollama did not answer or does not
have the model; `embedder not on GPU` means Ollama holds `bge-m3-8k:latest` on the
CPU or split between the card and system memory (typically because another
program took the card's memory after the model was unloaded); `myelin
unavailable` means the myelin server did not answer. The response does not give
the cause; the distill server's log does.

**Fix.** Read the log (`journalctl -u hs-serve-distill -n 100`), then follow
steps 1 and 2 of [the previous section](#a-scribe-or-distill-server-does-not-start).
Documents that arrived meanwhile wait on the queue; the watcher indexes them
when the server is ready again.

**Check it worked.** `curl -fsS http://localhost:7434/readiness` reports
`ready`.

## The dashboard says "backend unavailable: …"

**Symptom.** A scribe server's row in `hs status` reads like this, and
documents wait or move to another server:

```text
backend unavailable: 12.9 GiB free, needs 13.2 GiB; held by pid=4242 7534MB hs-distill-server; …
```

**Cause.** A scribe server that uses the olmocr converter runs its model on a
card that other programs share. It refuses work when the model is not loaded
and the card has less free memory than `scribe_server.vram_headroom_mb`. The
message is the diagnosis: how much is free, how much is needed, and which
processes hold the
rest. The gate is correct; the card is full.

**Fix.**

1. Read the holders in the message, or list them:

   ```sh
   nvidia-smi --query-compute-apps=pid,used_gpu_memory,process_name --format=csv
   ```

2. Stop or move the program that holds the most memory. Ollama keeps the
   embedding model loaded, so it holds part of the card for as long as the
   distill server is in use; stop or move whichever tenant you can spare.

**Check it worked.** The row changes to `running` or `idle`, and `curl -fsS
http://localhost:7433/readiness` reports free slots. How the gate works is in
[Sharing one GPU](../explanation/architecture.md#sharing-one-gpu).

## The Failed row is above zero

**Symptom.** `hs status` shows `Failed, retry` with a count and a reason, such
as `gapped_conversion`.

**Cause.** The documents were stamped as failed after every scribe tier gave up
on them. A **retryable** reason means a backend failed on the document, so a
fixed or different backend may convert it. A **bad source** reason means the
bytes cannot convert on any backend. Every reason is defined in the
[conversion failure reasons](../reference/status-dashboard.md#conversion-failure-reasons).

**Fix.**

1. See what would be re-queued:

   ```sh
   hs pipeline reconvert-failed --dry-run
   ```

2. Re-queue a few first, to size the work:

   ```sh
   hs pipeline reconvert-failed --limit 25 --yes
   ```

   Without `--limit`, the command re-queues every retryable document.

3. To re-queue a bad source after you fix its cause, name the reason:

   ```sh
   hs pipeline reconvert-failed --reason pdf_parse_error --dry-run
   ```

   Drop `--dry-run` to re-queue.

**Check it worked.** The Failed count falls as the Markdown count rises.

## The Stalled row is above zero

**Symptom.** `hs status` shows `Stalled` with a count and
`no markdown and not queued`.

**Cause.** These sources have no markdown, no failure stamp, and no message
waiting for a worker: they were never queued, or their message was lost.

**Fix.**

1. Check that the scribe watcher runs on the watcher host:

   ```sh
   systemctl --user status hs-scribe-watch-events
   ```

2. See what would be queued, then queue it:

   ```sh
   hs pipeline catch-up --dry-run
   hs pipeline catch-up
   ```

   The command re-announces every source that has no markdown and never deletes
   anything. It skips documents stamped as failed; Failed documents take
   `hs pipeline reconvert-failed`.

**Check it worked.** Stalled falls to zero as Queued rises and Markdown follows.

## A stream or consumer is stuck

**Symptom.** A watcher refuses to start and names a JetStream consumer setting
that differs from your configuration, or events stop moving although the
broker is healthy.

**Cause.** A stream or durable consumer holds an old configuration that
JetStream cannot change in place.

**Fix.**

1. Restore the old setting in `events.nats`, or reset the streams. A reset
   deletes the `PAPERS`, `SCRIBE` and `DISTILL` streams and recreates them from
   your configuration, and it discards every queued and in-flight event:

   ```sh
   hs pipeline events-reset
   ```

2. Re-queue what is missing:

   ```sh
   hs pipeline catch-up
   ```

**Check it worked.** The watchers start, and `hs status` shows Queued moving.

## The gateway reports an unreachable backend

**Symptom.** At startup the gateway logs a warning, and every request for that
service times out after seven seconds:

```text
WARN  docs backend http://192.0.2.10:7446 is not reachable (timed out
      — the host did not answer (a firewall that drops the port?)) — every
      request to `docs` will fail until it is
```

**Cause.** The service is healthy on its own host, so every check you run there
passes, but a firewall on that host drops the port for everyone else. The
gateway probes from its own host, which is the only place that shows it.

**Fix.**

1. On the host that serves the port, allow the LAN:

   ```sh
   sudo ufw allow from <lan-cidr> to any port 7446 proto tcp
   ```

   Use the port the warning names. The ports are in the
   [services reference](../reference/services-and-ports.md#ports).

2. Restart the gateway so that it probes again:

   ```sh
   sudo systemctl restart hs-gateway
   ```

**Check it worked.** The startup log shows no warning, and `curl -fsS
http://<gpu-host>:7446/health` answers from `<gateway-host>`.

## The gateway does not start

**Symptom.** `hs-gateway` exits at once, and `journalctl -u hs-gateway` shows
an error.

**Cause.** The gateway checks its configuration before it listens. It stops
when `cloud.gateway.routes` has no backend for one of `scribe`, `distill`,
`mcp` or `docs` (the error names the line to add), when a key under it names
a service the gateway does not route (a `memory` route left over from an
earlier release is one: delete the line), when a key in
`cloud.gateway` does not exist, when `--gateway-url` is missing or does not
start with `https://`, and when `HS_BACKEND_TOKEN` is unset or shorter than 32
bytes.

**Fix.**

1. Read the error. Add the missing route or fix the key in
   `~/.home-still/config.yaml` on `<gateway-host>`, as in
   [step 13 of the cluster tutorial](../tutorials/home-cloud-cluster.md#13-run-the-gateway).
2. Check that the unit's `ExecStart` passes `--gateway-url https://<public-host>`
   and that `~/.home-still/secrets.env` holds `HS_BACKEND_TOKEN`.
3. Restart the gateway:

   ```sh
   sudo systemctl restart hs-gateway
   ```

**Check it worked.** `curl -fsS http://127.0.0.1:7440/health` answers `ok`.

## A request answers 401

**Symptom.** A client, a watcher or the gateway gets `401` from a backend, or a
watcher exits with an error that names `HS_BACKEND_TOKEN`.

**Cause.** The value of `HS_BACKEND_TOKEN` differs between hosts. Every host
that runs a server or a client of one must hold the same value. Only
`GET /health` and `GET /readiness` answer without it.

**Fix.**

1. Compare the value on `<gateway-host>`, each backend host and each client
   host without printing it:

   ```sh
   grep HS_BACKEND_TOKEN ~/.home-still/secrets.env | sha256sum
   ```

   The digests must match.
2. Copy the correct value to the hosts that differ, and restart their services
   with `hs restart`.

**Check it worked.** The watcher stays up, and `hs status` shows no refusal.

## The gateway answers 401 right after enrollment

**Cause.** The clocks of `<gateway-host>` and the enrolling device disagree, so
the new token looks expired or not yet valid.

**Fix.**

1. On `<gateway-host>`, synchronise the clock:

   ```sh
   sudo apt install -y chrony
   sudo chronyc -a 'burst 4/4'
   sudo chronyc -a makestep
   ```

2. Create a new code with `hs cloud invite --name <device>` and enroll again with
   `hs cloud enroll --gateway https://cloud.example.com`.

**Check it worked.** `hs cloud status` on the device shows it enrolled, and a
request through the gateway succeeds.

## NFS errors or a slow `markdown/` directory on a Mac

**Symptom.** You see kernel messages such as `nfs server …: resource
temporarily unavailable (jukebox)`, `hs status` stalls on its filesystem counts,
or Finder hangs when you open `markdown/`.

**Cause.** One or more of these:

1. The export uses `sync`, so every write waits for a USB SSD.
2. macOS mounts NFS with block sizes that are too small.
3. Spotlight indexes the share and reads every file.
4. The macOS NFS client cache is stale after an earlier error.
5. A crashed write left `.tmp*` files in `markdown/`.

**Fix.**

1. On `<storage-host>`, make the export `async` in `/etc/exports`, then apply it:

   ```text
   /mnt/share/home-still   <lan-cidr>(rw,async,no_subtree_check,all_squash,anonuid=1000,anongid=1000)
   ```

   ```sh
   sudo exportfs -ra
   ```

2. On the Mac, remount with 1 MB blocks. This also clears a stale cache:

   ```sh
   sudo umount -f /Volumes/share
   sudo mkdir -p /Volumes/share
   sudo mount_nfs -o resvport,rw,rsize=1048576,wsize=1048576,nolocks \
       <storage-host>:/mnt/share/home-still /Volumes/share
   ```

3. Stop Spotlight indexing the share:

   ```sh
   sudo mdutil -i off /Volumes/share
   ```

4. On `<storage-host>`, remove the leftovers from crashed writes:

   ```sh
   rm -f /mnt/share/home-still/markdown/.tmp* /mnt/share/home-still/markdown/._.tmp*
   ```

**Check it worked.** The kernel messages stop, and `hs status` loads.

## Finder still hangs on `markdown/`

**Cause.** Finder does more than list a directory: it builds previews and totals
sizes. With thousands of files in sharded subdirectories, it can stall even
when NFS is healthy. This is a macOS limit, and `hs` reads the directory
normally.

**Fix.**

1. Use the terminal: `ls /Volumes/share/markdown/` is fast.
2. In Finder, choose View, Show View Options, and clear "Calculate all sizes"
   and "Show icon preview".
3. Use List view (Cmd+2), and open a shard directly with Cmd+Shift+G.

**Check it worked.** The folder opens in a few seconds.

## `hs upgrade` restarts a scribe watcher on a laptop

**Cause.** The laptop's configuration lets it run a scribe server, so the
upgrade restarts the services that belong with one.

**Fix.** Set `scribe.local_server` to `false` in `~/.home-still/config.yaml` on
the laptop, and point it at the remote servers:

```yaml
scribe:
  local_server: false
  servers:
    - http://<gpu-host>:7433
```

**Check it worked.** The next `hs upgrade` leaves the laptop's services alone.

## Garage crashes on a Raspberry Pi

**Cause.** Garage v2.1.0 and earlier crash on a Pi with `SIGILL`.

**Fix.** Install Garage v2.2.0 or later, as in
[Set up Garage on the storage host](operate-a-cluster.md#set-up-garage-on-the-storage-host).

**Check it worked.** `garage --version` prints v2.2.0 or later, and the service
stays up.

## Container output drowns the real messages

**Cause.** On a client node, `hs` prints the output of the container runtime
that it calls, which the client has no use for.

**Fix.** Run the command with `--quiet`, which suppresses output that is not a
result:

```sh
hs upgrade --check --quiet
```

**Check it worked.** Only the command's result is printed.

## Check it worked

When every symptom above is fixed, `hs status` shows every service healthy, Failed
and Stalled at zero, and the Markdown and Embedded rows at 100 %.
