# Operate a cluster

This page collects the tasks you repeat on a running home-cloud cluster:
verify it, upgrade it, add a GPU node or a scribe worker, take a node out,
rebuild the search index, and manage the storage. Each section is a task with its own
steps and check.

## Before you start

- You built the cluster with
  [Build a home-cloud research cluster](../tutorials/home-cloud-cluster.md), so
  the hosts keep the roles that tutorial gives them: `<gateway-host>`,
  `<storage-host>`, `<gpu-host>` and `<laptop>`. The myelin server runs on
  `<gpu-host>`.
- You can sign in to each host over SSH. Commands that must run on one
  particular host say so.
- Over SSH, user services need `XDG_RUNTIME_DIR` set. Run
  `export XDG_RUNTIME_DIR=/run/user/$(id -u)` before `hs upgrade` or
  `systemctl --user`.

## Verify the cluster

A healthy cluster passes every check below. Run the checks for the hosts you
have.

1. On `<gateway-host>`, check the gateway and the tunnel:

   ```sh
   sudo systemctl status hs-gateway cloudflared
   curl -fsS http://127.0.0.1:7440/health
   curl -fsS https://cloud.example.com/health
   ```

   Both services are active, and both `/health` calls answer `ok`.

2. On `<storage-host>`, check the storage you use. For NFS:

   ```sh
   showmount -e localhost
   ls /mnt/share/home-still/{papers,markdown,catalog} | head
   ```

   For Garage:

   ```sh
   sudo systemctl status garage
   garage status
   garage stats
   curl -fsS http://localhost:3903/health
   ```

   Garage's health answers "Garage is fully operational". Check the broker too:

   ```sh
   docker ps --filter name=nats
   ```

3. On `<gpu-host>`, check myelin and Ollama:

   ```sh
   systemctl --user status myelin-mcp
   ollama ps
   ```

   The `myelin-mcp` service is active (the unit name is the one you gave it in
   [set up myelin](../tutorials/home-cloud-cluster.md#8-set-up-myelin-on-the-gpu-host);
   `myelin-mcp` has no `/health` route, so the distill server's health call
   below is the check that it answers). `ollama ps` lists `bge-m3:latest`
   with its processor as `100% GPU`.

4. On `<gpu-host>`, check the card and the servers:

   ```sh
   nvidia-smi
   curl -fsS http://localhost:7433/health
   curl -fsS http://localhost:7434/health
   curl -fsS http://localhost:7446/health
   hs distill status
   journalctl -u hs-serve-distill | grep -i 'myelin reachable\|embedding model on the GPU'
   ```

   `nvidia-smi` lists the processes that hold GPU memory, each health call
   answers with HTTP 200, and the distill server's log says that myelin is
   reachable and that the embedding model is on the GPU.

5. From any client, check the whole pipeline:

   ```sh
   hs status
   hs distill search "test query"
   ```

   The dashboard lists every server as healthy, and the search returns
   passages. The rows are described in the
   [status dashboard reference](../reference/status-dashboard.md).

## Upgrade the cluster

Upgrade clients first (they are the cheapest to roll back), then the GPU host,
and the gateway last, so that devices outside your network keep working until
the gateway restarts.

1. On each host, in that order, check what is available:

   ```sh
   hs upgrade --check
   ```

   The command reports whether a newer release exists. It installs nothing.

2. Install it:

   ```sh
   hs upgrade --pre --yes
   ```

   `hs upgrade` downloads each binary's archive and its `.sha256`, refuses a
   release that has no checksum, replaces `hs` and every installed companion
   (`hs-distill-server`, `hs-gateway`, `hs-mcp`, `hs-scribe-server`), restarts
   the services that run those binaries, and checks their health. It installs
   `hs` last, so a failed run leaves the old `hs` in place and you can run the
   command again. It also pulls and restarts the scribe containers that
   `~/.home-still/docker-compose.yml` manages (when `scribe.local_server` is
   on), and leaves an unchanged, healthy container running. It does not touch
   myelin or its Qdrant: update those with the tools you installed them with. The flags are in the [CLI reference](../reference/cli.md#hs-upgrade).

3. On `<gpu-host>`, confirm the servers came back:

   ```sh
   curl -fsS http://localhost:7433/health
   curl -fsS http://localhost:7434/health
   ```

4. On `<gateway-host>`, confirm the gateway runs the new binary:

   ```sh
   sudo systemctl status hs-gateway
   curl -fsS https://cloud.example.com/health
   ```

5. On every laptop where Claude Desktop or Claude Code runs `hs-mcp` itself
   (installed by `hs mcp install`), upgrade the laptop with step 2, then quit and
   reopen the client so that it starts the new `hs-mcp`. A client that keeps
   the old binary lists the old tools.

Install new versions only with `hs upgrade`. Never copy a binary you built
yourself to a host.

### Check it worked

`hs --version` prints the same release on every host, and `hs status` shows
every service healthy.

## Restart services after changing a macOS LaunchAgent

`hs restart` and `hs upgrade` restart every home-still LaunchAgent by unloading
and loading it again from `~/Library/LaunchAgents/<label>.plist`, so an edit to
the plist (an environment variable, an argument) takes effect on the next
restart.

1. Edit the plist.
2. Run:

   ```sh
   hs restart
   ```

The command restarts the system services, daemons and containers it finds. If
the job's program is not the binary named in the plist, the restart fails with
a message naming both paths.

## Add a GPU node

A second GPU host adds conversion and indexing capacity.

1. On the new host, install home-still and save the shared secret as in steps 1
   and 2 of the
   [cluster tutorial](../tutorials/home-cloud-cluster.md#1-install-home-still-on-the-gateway-host-the-gpu-host-and-the-laptop).
2. Set `distill_server.myelin_url` in `~/.home-still/config.yaml` to the
   myelin server's MCP endpoint, and prepare the card, the layout models,
   pdfium and the scribe server as in steps 6 and 8 of the tutorial.
3. Start the servers as services:

   ```sh
   hs serve scribe --install
   hs serve distill --install
   ```

4. On `<gateway-host>`, add the node's URLs to the lists for `scribe` and
   `distill` in `cloud.gateway.routes`, then restart the gateway:

   ```sh
   sudo systemctl restart hs-gateway
   ```

5. On the host that runs the watchers, add the node's URLs to `scribe.servers`
   and `distill.servers`, then restart the watchers:

   ```sh
   systemctl --user restart hs-scribe-watch-events hs-distill-watch-events
   ```

### Check it worked

`hs status` lists the new servers under Services, and the gateway's startup log
shows no unreachable backend. The gateway spreads requests across the
instances of a route; see the
[gateway endpoints](../reference/services-and-ports.md#proxied-paths).

## Add a scribe worker

A scribe worker only converts documents. Set one up as in
[step 17 of the tutorial](../tutorials/home-cloud-cluster.md#17-add-a-second-scribe-worker),
then list its URL under `scribe.servers` on the watcher host and restart the
scribe watcher.

Never run a watcher, the distill server or the inbox on a worker. They write to
the storage and the catalog, which a worker does not reach.

### Check it worked

`hs status` lists the worker under Services.

## Bound a scribe server's memory

A PDF built to expand (a compressed object stream) can make pdfium use more
than a thousand times its file size in RAM: a 1.5 MB file has cost about
2.5 GB. Cap each scribe unit so that such a file kills one unit, not the
host.

1. Open an override for the unit:

   ```sh
   sudo systemctl edit hs-serve-scribe
   ```

2. Set `MemoryMax` to the host's RAM minus what its other tenants need (the
   VLM backend, the distill server), and `MemoryHigh` about 10 % below it,
   for example on a 32 GB host:

   ```ini
   [Service]
   MemoryMax=16G
   MemoryHigh=14G
   ```

3. Restart the unit:

   ```sh
   sudo systemctl restart hs-serve-scribe
   ```

Do the same for the scribe watcher's user unit
(`systemctl --user edit hs-scribe-watch-events`).

### Check it worked

```sh
systemctl show hs-serve-scribe -p MemoryMax -p MemoryHigh
```

It prints the values you set.

## Take a node out for maintenance

1. On `<gateway-host>`, remove the node's URL from its list in
   `cloud.gateway.routes`. An instance that refuses connections is skipped for
   `backend_failure_cooldown_secs` (10 by default), so stopping the node also
   works for a short job.
2. Restart the gateway:

   ```sh
   sudo systemctl restart hs-gateway
   ```

3. On the watcher host, remove the URL from `scribe.servers` or
   `distill.servers` and restart the watchers.

### Check it worked

`hs status` does not list the node, and requests through the gateway succeed.

## Rebuild a damaged search index

The markdown is the source of truth, so you can rebuild a store's records from
it. Use this when a store's records are damaged or lost, for example after
you replaced myelin's Qdrant volume. The conversion is not repeated.

1. On `<gpu-host>`, make sure myelin, Ollama and the distill server are
   running (see [Verify the cluster](#verify-the-cluster)).

2. If the store still holds records you want to discard, empty it. The call
   deletes every document of one store, and the reference lists that belong
   to it; it asks for no confirmation:

   ```sh
   curl -fsS -X POST -H "Authorization: Bearer $HS_BACKEND_TOKEN" \
     "http://localhost:7434/store/reset?store=academic"
   ```

   The answer names the store and how many chunks were deleted.

3. Index the markdown that has no records:

   ```sh
   hs distill reconcile --reembed
   ```

   To index documents that are already indexed again, replacing their records
   in place, run `hs distill reindex --all` instead; it can be resumed with
   `--older-than`. See the [CLI reference](../reference/cli.md#hs-distill).

4. Fill the other stores that are built from the catalog and from personal
   documents:

   ```sh
   hs distill abstracts build --force
   hs personal reindex <stem>
   ```

   Run `hs personal reindex` once for each personal document (`hs personal
   list` shows the stems).

Indexing a document writes its new records before it deletes the old ones, so
you can stop any of these commands and run it again.

### Check it worked

`hs distill status` shows the store's document and record counts rising
again, and `hs distill search "test query"` returns passages.

## Choose a storage backend

The storage is either an NFS export that every host mounts, or Garage, an
S3-compatible store that every host reaches over the network. Both hold the
same objects.

| | NFS | Garage S3 |
|---|---|---|
| Setup | One `/etc/exports` line. | A bucket, a key and an RPC secret. |
| Latency on the LAN | Lowest. | Slightly higher, from the S3 API. |
| Writes from several machines | NFS locking arbitrates. | The last writer wins; there are no S3 locks. |
| macOS | Native, but Finder is fragile. | `hs` talks S3 directly; mount it with `rclone nfsmount` only to browse files. |
| Across sites | Slow over a WAN. | The S3 API works from anywhere. |
| Durability | Whatever the disk gives you. | Block-level deduplication, and replication when you add nodes. |
| `hs` configuration | `storage.backend: local` | `storage.backend: s3` |
| Best for | One household and one SSD. | Several sites, future redundancy, or S3 clients. |

## Set up Garage on the storage host

Garage v2.2.0 or later is required: earlier versions crash on a Raspberry Pi
(SIGILL). These steps run on `<storage-host>` and keep NFS running, so you can
set up Garage first and move the data later.

1. Install Garage:

   ```sh
   sudo wget https://garagehq.deuxfleurs.fr/_releases/v2.2.0/aarch64-unknown-linux-musl/garage \
     -O /usr/local/bin/garage
   sudo chmod +x /usr/local/bin/garage
   garage --version
   ```

2. Generate its three secrets, and keep them in a password manager:

   ```sh
   RPC_SECRET=$(openssl rand -hex 32)
   ADMIN_TOKEN=$(openssl rand -base64 32)
   METRICS_TOKEN=$(openssl rand -base64 32)
   echo "RPC_SECRET=$RPC_SECRET"
   echo "ADMIN_TOKEN=$ADMIN_TOKEN"
   echo "METRICS_TOKEN=$METRICS_TOKEN"
   ```

3. Write `/etc/garage.toml`, replacing the three placeholders with the values
   from step 2:

   ```toml
   metadata_dir          = "/var/lib/garage/meta"
   data_dir              = "/mnt/share/garage/data"
   metadata_snapshots_dir = "/mnt/share/garage/snapshots"

   # SQLite is the safer engine on a Pi without a UPS: LMDB can corrupt on an
   # unclean shutdown, and SQLite recovers from its write-ahead log.
   db_engine             = "sqlite"
   metadata_auto_snapshot_interval = "6h"
   metadata_fsync        = true

   replication_factor    = 1
   compression_level     = 1
   block_size            = "1M"
   block_ram_buffer_max  = "128MiB"

   rpc_bind_addr   = "[::]:3901"
   rpc_public_addr = "127.0.0.1:3901"
   rpc_secret      = "<RPC_SECRET>"

   [s3_api]
   s3_region    = "garage"
   api_bind_addr = "[::]:3900"
   root_domain  = ".s3.garage.localhost"

   [s3_web]
   bind_addr    = "[::]:3902"
   root_domain  = ".web.garage.localhost"
   index        = "index.html"

   [admin]
   api_bind_addr = "[::]:3903"
   admin_token   = "<ADMIN_TOKEN>"
   metrics_token = "<METRICS_TOKEN>"
   ```

4. Run Garage as a service:

   ```sh
   sudo tee /etc/systemd/system/garage.service > /dev/null <<'EOF'
   [Unit]
   Description=Garage data store
   After=network-online.target
   Wants=network-online.target

   [Service]
   Environment='RUST_LOG=garage=info' 'RUST_BACKTRACE=1' 'GARAGE_LOG_TO_JOURNALD=true'
   ExecStart=/usr/local/bin/garage server
   StateDirectory=garage
   LimitNOFILE=42000

   [Install]
   WantedBy=multi-user.target
   EOF

   sudo mkdir -p /mnt/share/garage/{data,snapshots}
   sudo systemctl daemon-reload
   sudo systemctl enable --now garage
   sudo journalctl -u garage -f
   ```

5. Assign the node a role, then create the buckets and a key. Replace `563e`
   with the prefix `garage status` prints for your node:

   ```sh
   garage status
   garage layout assign -z dc1 -c 1500G 563e
   garage layout apply --version 1

   garage bucket create home-still
   garage key create home-still-app
   garage bucket allow --read --write --owner home-still --key home-still-app

   garage bucket create logs
   garage bucket allow --read --write --owner logs --key home-still-app
   ```

   `garage status` first lists the node with `NO ROLE ASSIGNED`. `garage key
   create` prints a key ID and a secret key. Save both: every host that uses the
   storage needs them.

### Check it worked

```sh
curl -fsS http://localhost:3903/health
aws --endpoint-url http://<storage-host>:3900 --region garage s3 ls
```

The health call answers "Garage is fully operational", and `aws` (with the key
configured) lists the `home-still` and `logs` buckets. If the host runs `ufw`,
allow ports 3900 and 3903 from `<lan-cidr>`.

## Move from NFS to Garage

Copy the data, then point every host at the bucket. Do the steps in order.

1. On `<gpu-host>`, stop the watchers so that nothing writes during the copy:

   ```sh
   systemctl --user stop hs-scribe-watch-events hs-distill-watch-events
   ```

   Stop the inbox watcher on your laptop with `hs scribe inbox uninstall`; you
   install it again in step 16 of the tutorial.

2. On `<storage-host>`, configure an `rclone` remote for Garage in
   `~/.config/rclone/rclone.conf`, using the key from the Garage setup:

   ```ini
   [garage]
   type = s3
   provider = Other
   env_auth = false
   access_key_id = <key-id>
   secret_access_key = <secret-key>
   region = garage
   endpoint = http://localhost:3900
   force_path_style = true
   acl = private
   bucket_acl = private
   ```

   Protect it with `chmod 600 ~/.config/rclone/rclone.conf`.

3. Copy the corpus:

   ```sh
   rclone copy /mnt/share/home-still garage:home-still \
       --transfers 2 --checkers 8 --progress --fast-list --check-first \
       --log-file /tmp/migration.log --log-level INFO
   ```

   `--transfers 2` limits random I/O on the one disk.

4. On every host that runs a home-still program (`<gpu-host>` and `<laptop>`), save the key in `~/.home-still/secrets.env`:

   ```sh
   echo 'HS_S3_ACCESS_KEY=<key-id>' >> ~/.home-still/secrets.env
   echo 'HS_S3_SECRET_KEY=<secret-key>' >> ~/.home-still/secrets.env
   ```

   Then replace the `home:` and `storage:` sections in
   `~/.home-still/config.yaml` with this one:

   ```yaml
   storage:
     backend: s3
     s3:
       endpoint: http://<storage-host>:3900
       bucket: home-still
       region: garage
       access_key: ${HS_S3_ACCESS_KEY}
       secret_key: ${HS_S3_SECRET_KEY}
       allow_http: true
   ```

5. Restart the services on each host, and reinstall the laptop's inbox watcher:

   ```sh
   hs restart
   hs scribe inbox install
   ```

6. When you trust the copy, retire the export on `<storage-host>` and reclaim
   the space:

   ```sh
   sudo exportfs -ua
   ```

Keys are described in the [configuration reference](../reference/configuration.md#storage).

### Check it worked

```sh
hs distill status
hs status
```

Both report the same documents and markdown as before the move. New downloads
appear in the bucket, not on the old share.

## Mount Garage on a Mac

You do not need a mount to use the library: `hs` talks S3 directly. Mount the
bucket only to browse the files in Finder. `rclone nfsmount` needs no kernel
extension.

1. Install `rclone` 1.65 or later (1.73 or later is recommended) and write the
   remote, with `http://<storage-host>:3900` as the endpoint, as in the copy
   step above:

   ```sh
   brew install rclone
   mkdir -p ~/.config/rclone && chmod 700 ~/.config/rclone
   ```

2. Mount the bucket:

   ```sh
   rclone lsd garage:
   mkdir -p ~/mnt/papers
   rclone nfsmount garage:home-still ~/mnt/papers \
       --vfs-cache-mode full \
       --vfs-cache-max-size 10G \
       --vfs-cache-max-age 48h \
       --vfs-fast-fingerprint \
       --use-server-modtime \
       --buffer-size 16M \
       --dir-cache-time 5m \
       --daemon
   ```

### Check it worked

`ls ~/mnt/papers` lists `papers`, `markdown` and `catalog`.

## Replace or rebalance a Garage node

For a single-node Garage, replace the node by swapping the SSD and restoring
from the most recent metadata snapshot in `/mnt/share/garage/snapshots`. A
cluster of several Garage nodes rebalances with `garage layout`; see Garage's
own documentation.

### Check it worked

`garage status` lists the node with its role, and `curl -fsS
http://localhost:3903/health` answers "Garage is fully operational".
