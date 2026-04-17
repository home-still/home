# home-still: home-cloud deployment guide

> **Goal: stand up a personal research engine across a small home cluster — Raspberry Pis plus one Linux GPU box — for under a few hundred dollars in hardware.** This guide is the recipe. Every command is concrete; every placeholder is clearly marked.

This guide assumes a sanitized topology with role-named hosts (`big`, `one`, `two`, `three`, `four`, `five`, `big_mac`, `mac_air`) on a private LAN. **Replace placeholders, never paste real IPs/hostnames/secrets/tunnel-IDs into a public copy of this doc.**

---

## 1. Why this topology exists

The split into "tunnel host", "storage server", "GPU compute", "DB host", and "clients" isn't a flex. Each role exists because:

- **Cheap hardware doesn't have everything in one box.** A Pi 5 can serve files all day but can't embed millions of vectors. A used GPU desktop can crunch vectors but you don't want it on 24/7 holding your only copy of the corpus.
- **You want to reach your library from outside.** That requires *one* machine talking to Cloudflare, not all of them.
- **You want failure to be local.** If the GPU box dies, the storage server still has your papers. If the storage server dies, the GPU box still works on whatever it was indexing.
- **You want to grow.** Adding a second GPU compute node should be a single command, not a re-architecture.

Every machine in this guide is doing the smallest job that makes sense for the hardware it has. That's the entire design philosophy.

## 2. Reference topology

```
                       Internet
                          │
                          ▼
                   ┌──────────────┐
                   │  Cloudflare  │
                   │   Edge DFW   │
                   └──────┬───────┘
                          │ QUIC tunnel
                          ▼
   ┌──────────────────────────────────────────────────────────┐
   │                    home LAN  (.0/24)                     │
   │                                                          │
   │   two (.102)        three (.103)      four (.104)        │
   │   ─────────         ──────────        ─────────          │
   │   tunnel host       storage server    DB host            │
   │   cloudflared       NFS / Garage S3   Postgres 17        │
   │   hs-gateway        T7 SSD 1.8 TB     Qdrant             │
   │   Pi 4 8 GB         Pi 5 8 GB         Pi 5 16 GB / NVMe  │
   │                                                          │
   │   one (.101)        big (.110)        five (.105)        │
   │   ─────────         ─────────         ─────────          │
   │   nginx / web       GPU compute       OFFLINE / spare    │
   │   Pi 5 8 GB         hs-scribe                            │
   │                     hs-distill                           │
   │                     CUDA, ≥8 GB VRAM                     │
   │                                                          │
   │   big_mac (.111)    mac_air (.112)                       │
   │   ──────────        ──────────                           │
   │   SSH jump host     daily-driver client                  │
   │   admin only        hs CLI, Claude                       │
   └──────────────────────────────────────────────────────────┘
```

Real-world constraints to keep in mind:

- The **storage server** owns the only canonical copy of your corpus. Plan a backup strategy before you trust it with months of work.
- The **GPU compute** box is the only machine where CUDA matters. Other hosts can run on Pi-class hardware.
- The **DB host** could double as the storage server on a tiny setup, but separating them keeps Qdrant's I/O off the same disk as the markdown corpus.
- The **tunnel host** must be the only machine reachable from outside. Nothing else gets a public hostname.

## 3. What each role does

### Tunnel host (`two`)
Runs `cloudflared` and `hs-gateway`. Cloudflared maintains a persistent QUIC connection out to Cloudflare's edge — no inbound ports on your router. `hs-gateway` validates bearer tokens (or runs the OAuth2 dance for Claude Desktop) and reverse-proxies legitimate requests to backend services on the LAN. **This is the only public surface.** A Pi 4 with 8 GB RAM is overkill for it.

### Storage server (`three`)
Holds papers, markdown, and catalog files on its USB SSD. Serves them via either NFS (default, simplest) or Garage S3 (S3-compatible, more flexible). Other machines mount it. A Pi 5 with 8 GB RAM and a Samsung T7 1.8 TB USB SSD is the standard build.

### GPU compute (`big`)
The only machine that needs serious horsepower. Runs `hs-scribe` (turns PDFs into markdown via VLM) and `hs-distill` (chunks markdown, embeds it with BGE-M3, and writes vectors to Qdrant). **CUDA is mandatory** for distill (project non-negotiable — see `CLAUDE.md`) and strongly preferred for scribe. An old gaming PC with an NVIDIA card and ≥8 GB VRAM does the job.

### DB host (`four`)
Runs **Qdrant** (vector database, the heart of distill) and **Postgres 17** (paper metadata, dedup tracking, and a slot for future analytics). Both want fast disk — NVMe over USB or M.2 HAT. A Pi 5 with 16 GB RAM and a 512 GB NVMe is the recommended build.

### Clients (`mac_air`, `big_mac`)
Your laptops. They run the `hs` CLI, mount the storage server (NFS or rclone-S3), and either talk directly to scribe/distill on the LAN or go through the gateway when off-network. They never run scribe or distill themselves — set `scribe.local_server: false` in their config.

## 4. Hardware minimums per role

| Role            | CPU                  | RAM     | Storage                   | GPU        |
|-----------------|----------------------|---------|---------------------------|------------|
| Tunnel host     | Any Pi 4 / Pi 5      | 4–8 GB  | 32 GB SD                  | none       |
| Storage server  | Pi 5                 | 8 GB    | 1–2 TB USB SSD            | none       |
| GPU compute     | Modern x86_64        | 32 GB   | 256 GB NVMe (model cache) | ≥8 GB VRAM, NVIDIA |
| DB host         | Pi 5                 | 16 GB   | 512 GB NVMe (HAT or USB)  | none       |
| Client          | anything             | 8 GB    | 64 GB free                | optional   |

The GPU box is the only one you can't substitute Pi-class hardware for. Everything else can be a used Raspberry Pi 4 or 5 from your closet.

## 5. Network plan

All ports below are LAN-only **except** the gateway, which exits the LAN through the Cloudflare tunnel rather than via direct port forwarding.

| Port  | Service           | Lives on        | Connected from              | Public?   |
|-------|-------------------|-----------------|-----------------------------|-----------|
| 7433  | Scribe server     | `big`           | clients, gateway            | LAN only  |
| 7434  | Distill server    | `big`           | clients, gateway            | LAN only  |
| 7440  | hs-gateway        | `two`           | cloudflared (loopback)      | via tunnel |
| 7445  | MCP server        | `big` or `two`  | gateway                     | LAN only  |
| 6333  | Qdrant REST       | `four`          | distill server (`big`)      | LAN only  |
| 6334  | Qdrant gRPC       | `four`          | distill server (`big`)      | LAN only  |
| 11434 | Ollama (VLM)      | `big`           | scribe server (loopback)    | host only |
| 5432  | Postgres          | `four`          | scribe / distill / hs CLI   | LAN only  |
| 2049  | NFS               | `three`         | clients, `big`              | LAN only  |
| 3900  | Garage S3 API     | `three`         | clients, `big`              | LAN only  |
| 3901  | Garage RPC        | `three`         | other Garage nodes (none)   | LAN only  |
| 3903  | Garage admin/health | `three`       | `hs status`, monitoring     | LAN only  |
| 4222  | NATS (optional)   | `three`         | scribe / distill watchers   | LAN only  |

(Cross-check against the "Network ports" table in the root README — they should agree.)

A LAN firewall rule that only permits traffic *between* `192.168.1.0/24` hosts is enough. The tunnel host is the only one that needs outbound 443 to `*.cloudflare.com`.

## 6. Per-host setup recipes

All recipes use the convention **"Run as: sudo bash"** or **"Run as: bash"** at the top of any script handed off, so there's no ambiguity about privileges.

### 6.1 Tunnel host (`two`)

**Install cloudflared**

Run as: `sudo bash`

```bash
# On Pi (ARM64):
curl -LO https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-arm64.deb
sudo dpkg -i cloudflared-linux-arm64.deb
cloudflared --version
```

**Authenticate and create the tunnel**

Run as: `bash` (interactive — opens a browser link to authorize)

```bash
cloudflared tunnel login
cloudflared tunnel create home-still
# Note the tunnel UUID it prints. Replace <TUNNEL_ID> below.
```

The credential file lands at `~/.cloudflared/<TUNNEL_ID>.json`. Keep it secret.

**Configure ingress**

Write `~/.cloudflared/config.yml`:

```yaml
tunnel: <TUNNEL_ID>
credentials-file: /home/<your-user>/.cloudflared/<TUNNEL_ID>.json

ingress:
  - hostname: cloud.example.com
    service: http://127.0.0.1:7440
    originRequest:
      connectTimeout: 30s
      keepAliveTimeout: 600s
  - service: http_status:404
```

Route DNS:

```bash
cloudflared tunnel route dns home-still cloud.example.com
```

**Install hs-gateway**

Run as: `bash`

```bash
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
hs cloud init   # generates ~/.home-still/cloud-secret.key
```

**Configure the gateway**

Edit `~/.home-still/config.yaml`:

```yaml
cloud:
  role: gateway
  gateway_url: https://cloud.example.com
  gateway:
    listen: 127.0.0.1:7440
    secret_path: /home/<your-user>/.home-still/cloud-secret.key
    token_ttl_secs: 14400      # 4 hours
    refresh_ttl_secs: 604800   # 7 days
    routes:
      scribe:  http://big:7433
      distill: http://big:7434
      mcp:     http://127.0.0.1:7445
```

**Run as systemd services**

Run as: `sudo bash`

```bash
sudo systemctl enable --now cloudflared

sudo tee /etc/systemd/system/hs-gateway.service > /dev/null <<'EOF'
[Unit]
Description=home-still cloud gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=<your-user>
ExecStart=/home/<your-user>/.local/bin/hs-gateway --gateway-url https://cloud.example.com
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now hs-gateway
sudo journalctl -u hs-gateway -f
```

**Verify**

```bash
curl -fsS http://127.0.0.1:7440/health
curl -fsS https://cloud.example.com/health
```

Both should return `200 OK`.

### 6.2 Storage server (`three`)

You have two paths. Pick one (or run both during a migration).

#### Path A — NFS (simplest)

**Prepare the mount**

Run as: `sudo bash`

```bash
# T7 USB SSD assumed at /mnt/codex_fs (ext4 or xfs)
sudo mkdir -p /mnt/codex_fs/home-still/{papers,markdown,catalog,logs}
sudo chown -R 1000:1000 /mnt/codex_fs/home-still
```

> **Important:** never run `chown -R` from a parent directory that has foreign mounts nested inside (e.g. `~/mnt`, `/Volumes`, `/mnt`). Recurse only inside the directory you actually own.

**Configure exports**

Edit `/etc/exports`:

```
/mnt/codex_fs/home-still   192.168.1.0/24(rw,async,no_subtree_check,all_squash,anonuid=1000,anongid=1000)
```

The `async` flag is critical — without it, every write blocks on disk and a USB SSD will crawl. (Symptom: jukebox errors, Mac Finder hangs. See the root README's NFS troubleshooting section.)

Apply and enable:

```bash
sudo exportfs -ra
sudo systemctl enable --now nfs-server
showmount -e localhost
```

#### Path B — Garage S3 (more flexible)

**Install Garage v2.2.0**

Run as: `sudo bash`

```bash
sudo wget https://garagehq.deuxfleurs.fr/_releases/v2.2.0/aarch64-unknown-linux-musl/garage \
  -O /usr/local/bin/garage
sudo chmod +x /usr/local/bin/garage
garage --version
```

> v2.2.0 includes the SIGILL fix (#1217) for Raspberry Pi. Older versions crash on ARM64. Do not substitute v1.x or earlier v2.x.

**Generate secrets**

```bash
RPC_SECRET=$(openssl rand -hex 32)
ADMIN_TOKEN=$(openssl rand -base64 32)
METRICS_TOKEN=$(openssl rand -base64 32)
echo "RPC_SECRET=$RPC_SECRET"
echo "ADMIN_TOKEN=$ADMIN_TOKEN"
echo "METRICS_TOKEN=$METRICS_TOKEN"
```

Save these somewhere safe (a password manager, not the repo).

**Configure Garage**

Write `/etc/garage.toml`:

```toml
metadata_dir          = "/var/lib/garage/meta"
data_dir              = "/mnt/codex_fs/garage/data"
metadata_snapshots_dir = "/mnt/codex_fs/garage/snapshots"

# SQLite is the safer engine for a Pi without a UPS — LMDB can corrupt on
# unclean shutdown. SQLite WAL recovers automatically.
db_engine             = "sqlite"
metadata_auto_snapshot_interval = "6h"
metadata_fsync        = true

replication_factor    = 1
compression_level     = 1
block_size            = "1M"
block_ram_buffer_max  = "128MiB"

rpc_bind_addr   = "[::]:3901"
rpc_public_addr = "127.0.0.1:3901"
rpc_secret      = "<RPC_SECRET from above>"

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
admin_token   = "<ADMIN_TOKEN from above>"
metrics_token = "<METRICS_TOKEN from above>"
```

**Systemd unit**

```bash
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

sudo mkdir -p /mnt/codex_fs/garage/{data,snapshots}
sudo systemctl daemon-reload
sudo systemctl enable --now garage
sudo journalctl -u garage -f
```

**Initialize the cluster, bucket, and key**

Run as: `bash`

```bash
garage status
# → 563e1ac825ee3323   three   127.0.0.1:3901   NO ROLE ASSIGNED

# Replace 563e with the prefix from your status output
garage layout assign -z dc1 -c 1500G 563e
garage layout apply --version 1

garage bucket create home-still
garage key create home-still-app
# Save the printed Key ID + Secret key — you'll need them on every client.
garage bucket allow --read --write --owner home-still --key home-still-app

# Also create a logs bucket for centralized JSONL log shipping:
garage bucket create logs
garage bucket allow --read --write --owner logs --key home-still-app
```

**Verify**

```bash
curl -fsS http://localhost:3903/health
# → "Garage is fully operational"

# From a client with awscli installed:
aws --endpoint-url http://three:3900 \
    --region garage \
    s3 ls
```

### 6.3 GPU compute (`big`)

This is the only machine where the install is non-trivial.

**Install NVIDIA driver and CUDA**

Run as: `sudo bash` (Arch / EndeavourOS / CachyOS)

```bash
sudo pacman -S nvidia nvidia-utils cuda cudnn
nvidia-smi   # confirms driver is loaded and GPU is detected
nvcc --version
```

For Ubuntu/Debian, use the upstream NVIDIA repo per CUDA's official install guide. **Do not** mix distro packages with NVIDIA's `.run` installer.

**Resolve the onnxruntime CUDA provider library**

The pyke ort cache (`~/.cache/ort.pyke.io/dfbin/`) sometimes ships a CUDA-12-only bundle on a CUDA-13 host. Symptom: distill silently falls back to CPU and your VRAM probe in the logs reads `0 MB used`.

Run as: `bash`

```bash
ls ~/.cache/ort.pyke.io/dfbin/
ldd ~/.cache/ort.pyke.io/dfbin/<hash>/libonnxruntime_providers_cuda.so | grep "not found"
# If anything reports "not found", remove that hash directory:
rm -rf ~/.cache/ort.pyke.io/dfbin/<bad-hash>
```

Then ensure CUDA's lib path is exported in your shell profile:

```bash
echo 'set -gx LD_LIBRARY_PATH /opt/cuda/lib64 $LD_LIBRARY_PATH' >> ~/.config/fish/config.fish
# or, for bash/zsh:
echo 'export LD_LIBRARY_PATH=/opt/cuda/lib64:$LD_LIBRARY_PATH' >> ~/.bashrc
```

**Mount the storage server**

For NFS:

```bash
sudo mkdir -p /mnt/codex_fs
echo 'three:/mnt/codex_fs/home-still  /mnt/codex_fs  nfs4  rw,async,vers=4,_netdev  0  0' \
  | sudo tee -a /etc/fstab
sudo mount /mnt/codex_fs
ls /mnt/codex_fs/home-still
```

For Garage S3, configure the `hs` storage backend instead (see step below) — `big` does not need a filesystem mount when using S3.

**Install hs and bring up the services**

Run as: `bash`

```bash
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
hs config init
```

Edit `~/.home-still/config.yaml` — set `home.project_dir` to the NFS mount path or set the storage backend to `s3`:

```yaml
home:
  project_dir: /mnt/codex_fs/home-still   # for NFS
  log_dir: /mnt/codex_fs/home-still/logs

storage:
  backend: local                          # or s3 — see below

# When using Garage S3, replace the local block with:
# storage:
#   backend: s3
#   s3:
#     endpoint: http://three:3900
#     bucket: home-still
#     region: garage
#     access_key: ${HS_S3_ACCESS_KEY}
#     secret_key: ${HS_S3_SECRET_KEY}
#     allow_http: true

distill_server:
  host: 0.0.0.0
  port: 7434
  qdrant_url: http://four:6334
  qdrant_data_dir: /var/lib/qdrant      # if Qdrant is local; otherwise unused
  collection_name: academic_papers

scribe:
  output_dir: /mnt/codex_fs/home-still/markdown
  watch_dir: /mnt/codex_fs/home-still/papers
  servers:
    - http://localhost:7433
  local_server: true                    # this host runs scribe
```

For S3 mode, export the credentials before starting any service:

```bash
set -Ux HS_S3_ACCESS_KEY GK351...
set -Ux HS_S3_SECRET_KEY 7d37...
```

**Initialize and start scribe**

```bash
hs scribe init                # downloads layout model, sets up Ollama + scribe container
hs scribe server start
hs scribe server ping
```

> Suppress non-actionable container output on this node — Docker/podman are noisy with health-check chatter that is not actionable. Pipe through `grep -v` or run with `--quiet` where the CLI supports it.

**Initialize and start distill — with CUDA**

```bash
hs distill init               # creates the Qdrant compose file (skip if Qdrant runs on `four`)
hs distill server start
hs distill server ping
```

If you're building distill from source rather than using the prebuilt binary, **always pass `--features cuda`**:

```bash
cargo build --release -p hs-distill --features server,cuda
```

Without `--features cuda`, distill silently falls back to CPU, which at corpus scale degrades to unusable. Confirm CUDA is in use by inspecting the server logs for the VRAM probe — non-zero VRAM means the GPU is in play.

For local rc deploys, set the version explicitly so `git describe` doesn't bake the prior tag:

```bash
GITHUB_REF_NAME=v0.0.1-rc.NNN cargo build --release -p hs-distill --features server,cuda
```

**Register with the gateway as a serve-mode service**

```bash
hs cloud enroll --gateway https://cloud.example.com
# enter the enrollment code printed on `two` by `hs cloud invite --name big`

hs serve scribe   # one terminal — auto-init, start, and register
hs serve distill  # another terminal — same
```

Once registered, the gateway routes `/scribe/*` and `/distill/*` to this host even when off-network clients connect through Cloudflare.

### 6.4 DB host (`four`)

Two services live here: **Postgres 17** and **Qdrant**.

#### Postgres 17

Run as: `sudo bash`

```bash
sudo apt update
sudo apt install -y postgresql-17 postgresql-contrib-17
sudo systemctl enable --now postgresql
```

**Move the data directory to NVMe**

```bash
sudo systemctl stop postgresql
sudo rsync -a /var/lib/postgresql/17/main/ /mnt/nvme/postgres/17/main/
sudo chown -R postgres:postgres /mnt/nvme/postgres
# Edit /etc/postgresql/17/main/postgresql.conf:
#   data_directory = '/mnt/nvme/postgres/17/main'
#   listen_addresses = '*'
sudo systemctl start postgresql
```

**Configure LAN-only access**

Edit `/etc/postgresql/17/main/pg_hba.conf`:

```
# IPv4 local connections (LAN only):
host    all   all   192.168.1.0/24   scram-sha-256
```

Reload:

```bash
sudo -u postgres psql -c "SELECT pg_reload_conf();"
```

**Create the home-still role and database**

Run as: `bash`

```bash
sudo -u postgres psql <<'SQL'
CREATE ROLE home_still WITH LOGIN PASSWORD 'CHANGE_ME';
CREATE DATABASE home_still OWNER home_still;
GRANT ALL PRIVILEGES ON DATABASE home_still TO home_still;
SQL
```

**Intended use of the Postgres slot**

home-still does not yet consume Postgres directly — but `four` is the planned home for:
- **Paper metadata** (DOI, title, authors, year, citations) cached from the six providers, so search doesn't hammer the upstream APIs.
- **Dedup tracking** of downloaded PDFs (which DOI has been fetched, when, from which provider).
- **Scribe + distill job ledger** for retry/resume logic across the cluster.
- **Future analytics** (which queries return useful results, which papers get re-read).

Reserve the database now so future features have a home; the schema will land as features ship.

**Backup**

```bash
# Hourly logical dump to the NFS share:
sudo tee /etc/cron.hourly/pgdump > /dev/null <<'EOF'
#!/bin/bash
set -eu
TS=$(date +%Y%m%d-%H%M)
sudo -u postgres pg_dump home_still | gzip > /mnt/codex_fs/home-still/backups/postgres/home_still-$TS.sql.gz
find /mnt/codex_fs/home-still/backups/postgres -name 'home_still-*.sql.gz' -mtime +14 -delete
EOF
sudo chmod +x /etc/cron.hourly/pgdump
```

#### Qdrant

Run as: `bash`

```bash
mkdir -p /mnt/nvme/qdrant
docker run -d \
  --name qdrant \
  --restart unless-stopped \
  -p 6333:6333 -p 6334:6334 \
  -v /mnt/nvme/qdrant:/qdrant/storage \
  qdrant/qdrant:latest

curl -fsS http://localhost:6333/healthz
curl -fsS http://localhost:6333/collections
```

**Sizing guidance** (per `slop/2026-03-17-init.md`):

- Each 768-dim vector with scalar quantization costs ~4.2 KB on disk in Qdrant (original + quantized + HNSW links + payload).
- A T1+T2+T3 ingest (OpenAlex abstracts + PMC OA full text + 10 M filtered CORE papers) lands at ~361 M vectors / ~1.5 TB.
- For 32 GB RAM, prefer scalar quantization (SQ int8) and mmap originals. For 16 GB or less, drop HNSW `m` to 8 or use product quantization.
- Set `on_disk: true` on the collection's vector config and let mmap do the work.

**Allow distill on `big` to reach Qdrant**

Confirm port 6334 is open from `big` (it should be — same LAN). Then on `big`'s `~/.home-still/config.yaml`, set:

```yaml
distill_server:
  qdrant_url: http://four:6334
```

### 6.5 Clients (`mac_air`, `big_mac`)

**Install hs**

Run as: `bash`

```bash
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
hs config init
```

**Mount the storage server**

For NFS on macOS — large block sizes are critical (default is too small and Finder will hang):

```bash
sudo mkdir -p /Volumes/codex_fs
sudo mount_nfs -o resvport,rw,rsize=1048576,wsize=1048576,nolocks \
    three:/mnt/codex_fs /Volumes/codex_fs

# Disable Spotlight indexing to keep Finder responsive:
sudo mdutil -i off /Volumes/codex_fs
```

For Garage S3 on macOS via `rclone nfsmount` (no FUSE / kext needed):

```bash
brew install rclone   # ≥ v1.65.0; v1.73+ recommended

mkdir -p ~/.config/rclone && chmod 700 ~/.config/rclone
cat > ~/.config/rclone/rclone.conf <<EOF
[garage]
type = s3
provider = Other
env_auth = false
access_key_id = <KEY_ID from garage key info>
secret_access_key = <SECRET from garage key info>
region = garage
endpoint = http://three:3900
force_path_style = true
acl = private
bucket_acl = private
EOF
chmod 600 ~/.config/rclone/rclone.conf

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

For automount on login, write a LaunchAgent plist — the slop note `slop/2026-04-15-mac-air-rclone-nfsmount-setup.md` has the full template.

**Configure the client to consume, not host, services**

Edit `~/.home-still/config.yaml`:

```yaml
home:
  project_dir: /Volumes/codex_fs/home-still   # or ~/mnt/papers for Garage

scribe:
  servers:
    - http://big:7433
  local_server: false       # this client must NOT try to start scribe locally

distill:
  servers:
    - http://big:7434
```

`local_server: false` is critical on a Mac client — without it, `hs upgrade` will try to restart a scribe watcher and Docker/podman setup that has no business running on a laptop.

**Enroll with the gateway** (so the laptop can reach the cluster from outside the LAN):

Run on `two`:

```bash
hs cloud invite --name mac_air
# Note the printed code, e.g. A7X-K9M (5-minute, single-use)
```

Run on `mac_air`:

```bash
hs cloud enroll --gateway https://cloud.example.com
# Paste the code when prompted
hs cloud status
```

Now `hs distill search` works locally (direct to `big:7434` on LAN) **and** remotely (through the gateway over the tunnel).

## 7. Storage backend deep-dive: NFS vs Garage S3

Both work. Pick based on what you actually need.

|                          | NFS                             | Garage S3                            |
|--------------------------|---------------------------------|--------------------------------------|
| **Setup complexity**     | Low — one `/etc/exports` line   | Moderate — bucket, key, RPC secret    |
| **Latency**              | Lowest on LAN                   | Slightly higher (S3 API overhead)    |
| **Cross-machine writes** | Handled by NFS locking          | Last-writer-wins (no S3 locks)       |
| **macOS support**        | Native, but Finder is fragile   | Via `rclone nfsmount` — kext-free    |
| **Multi-site / WAN**     | Painful (NFS over WAN is slow)  | Native S3 SDK from anywhere          |
| **Durability story**     | Whatever the underlying disk is | Block-level dedup, configurable replication when you add nodes |
| **Fits in `hs` config**  | `storage.backend: local`        | `storage.backend: s3`                |
| **Best fit**             | Single household, single SSD    | Multiple sites, future redundancy, want S3 SDK access from clients |

**Migration path NFS → Garage:**

1. Stand up Garage on `three` alongside NFS (both can run on the same host — different ports, different data dirs).
2. One-shot copy with rclone (run on `three`):
   ```bash
   rclone copy /mnt/codex_fs/home-still garage:home-still \
       --transfers 2 --checkers 8 --progress --fast-list --check-first \
       --log-file /tmp/migration.log --log-level INFO
   ```
   `--transfers 2` keeps random-I/O contention down on the same disk.
3. Flip `storage.backend: local` to `storage.backend: s3` on every host. Restart services. Verify with `hs distill status` and `hs status`.
4. Once confident, retire the NFS export (`exportfs -ua`) and reclaim the space.

For the deeper compatibility story (which S3 operations Garage supports, the `force_path_style` / `request_checksum_calculation` Rust SDK gotchas), see `slop/2026-04-15-switching-to-garage-s3-like-fs.md` and `slop/2026-04-15-nfs-mounting-s3-buckets-reliably.md`. They are *not* user-facing docs — they're the operational research that informs this guide.

## 8. Cloudflare tunnel + OAuth gateway

Once the gateway is running on `two` (step 6.1), exposing it to Claude Desktop or remote `hs` CLI takes three steps.

**Generate an enrollment code** (on `two`):

```bash
hs cloud invite --name <device-name>
# → prints a 5-minute, single-use code like "A7X-K9M"
```

**On the remote device:**

For the `hs` CLI:

```bash
hs cloud enroll --gateway https://cloud.example.com
# Paste the code
hs cloud status   # confirms enrollment
hs cloud token    # prints a 4-hour access token (refreshes automatically)
```

For Claude Desktop:

1. In Claude Desktop, add `https://cloud.example.com/mcp` as a remote MCP server.
2. Claude opens your browser to the gateway's authorization page.
3. Run `hs cloud invite --name claude-desktop` on `two` to generate a code.
4. Paste the code, click Authorize.
5. Claude Desktop stores the tokens and refreshes them using the 7-day refresh token.

The gateway implements **OAuth 2.1 Authorization Code + PKCE** (RFC 7591 dynamic client registration, RFC 8414 server metadata, RFC 9728 protected-resource metadata). For the full auth model — token format, scope checks, registry protocol — see `crates/hs-gateway/README.md`.

## 9. Verifying the cluster

A healthy cluster passes all of the following checks.

**On `two` (gateway):**

```bash
sudo systemctl status hs-gateway cloudflared
curl -fsS http://127.0.0.1:7440/health
curl -fsS https://cloud.example.com/health
```

**On `three` (storage):**

For NFS:

```bash
showmount -e localhost
ls /mnt/codex_fs/home-still/{papers,markdown,catalog} | head
```

For Garage:

```bash
sudo systemctl status garage
garage status
garage stats
curl -fsS http://localhost:3903/health
```

**On `four` (DB host):**

```bash
sudo systemctl status postgresql
sudo -u postgres psql -d home_still -c '\dt'

docker ps --filter name=qdrant
curl -fsS http://localhost:6333/healthz
curl -fsS http://localhost:6333/collections
```

**On `big` (GPU compute):**

```bash
nvidia-smi
hs scribe server ping
hs distill server ping
hs distill status
# Look for non-zero VRAM in distill server logs:
tail -n 50 ~/home-still/logs/distill-server.log | grep -i 'vram\|cuda\|provider'
```

**From any client:**

```bash
hs server list           # all registered services across the cluster
hs status                # live TUI dashboard
hs distill search "test query"
```

Expected output of `hs server list` on a healthy three-node cluster:

```
SERVICE   HOST    URL                       ENABLED  HEALTHY  AGE
scribe    big     http://big:7433           true     true     12s
distill   big     http://big:7434           true     true     14s
mcp       two     http://127.0.0.1:7445     true     true     8s
```

## 10. Common operational tasks

### Rolling upgrade

The order matters: clients first (cheapest to roll back), then compute, then the gateway last (so off-network clients keep working until the gateway flips).

On each host:

```bash
hs upgrade --check    # dry-run
hs upgrade            # download, swap binary, restart containers, health check
```

For `big`:

```bash
hs upgrade            # rolls scribe + distill in place; brief downtime per service
hs scribe server ping
hs distill server ping
```

For `two`:

```bash
hs upgrade
sudo systemctl restart hs-gateway
```

### Adding a new GPU compute node

On the new host:

```bash
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
hs config init
hs scribe init
hs distill init
hs cloud enroll --gateway https://cloud.example.com   # paste a fresh invite
hs serve scribe
hs serve distill
```

The new node auto-registers with the gateway and starts receiving load. `hs server list` from any host now shows it.

### Disabling a node for maintenance

```bash
hs server disable <hostname>:7433     # take scribe out of rotation
hs server disable <hostname>:7434     # take distill out of rotation
# ...do the maintenance...
hs server enable <hostname>:7433
hs server enable <hostname>:7434
```

The disabled service stays in the registry but is skipped during load balancing.

### Recovering a corrupted Qdrant collection

```bash
# On four:
docker exec qdrant rm -rf /qdrant/storage/collections/academic_papers
docker restart qdrant

# On big — re-index from markdown (idempotent, deterministic point IDs):
hs distill index
```

Indexing is content-addressed (xxhash + UUID v5 from the doc stem), so re-running it is safe and resumable.

### Postgres backup and restore

The hourly `pg_dump` cron from step 6.4 writes to `/mnt/codex_fs/home-still/backups/postgres/`. To restore:

```bash
# On four:
gunzip -c /mnt/codex_fs/home-still/backups/postgres/home_still-YYYYMMDD-HHMM.sql.gz \
  | sudo -u postgres psql -d home_still
```

### Garage node replacement / rebalance

For a single-node Garage, replacement = swap the SSD and rsync from the most recent metadata snapshot. Multi-node rebalancing is a `garage layout` operation — out of scope for this single-node guide; see Garage's own docs.

## 11. Troubleshooting

### Distill silently runs on CPU

Symptom: search latency in seconds instead of milliseconds; VRAM probe in distill server logs reads `0`.

Causes, in order of likelihood:

1. **Built without `--features cuda`** — rebuild with the feature flag. Project rule: this stays on, period.
2. **Pyke ort cache holds a CUDA-12-only bundle on a CUDA-13 host** — `ldd ~/.cache/ort.pyke.io/dfbin/<hash>/libonnxruntime_providers_cuda.so | grep "not found"`; if anything's missing, `rm -rf` that hash dir and let it re-download.
3. **`LD_LIBRARY_PATH` missing CUDA's lib dir** — `set -gx LD_LIBRARY_PATH /opt/cuda/lib64 $LD_LIBRARY_PATH` in fish, or `export` in bash.

**Do not** flip `compute_device: cuda` to anything else in the config. CPU embedding at corpus scale is unusable. Fix CUDA, don't bypass it.

### Scribe also silently on CPU

Same root cause family as distill — same fix. CUDA stays on for every GPU-accelerated path, not just embedding.

### NFS jukebox errors / Mac Finder hangs on `markdown/`

See the root README's "Mac Known Issues and Fixes" section for the full recipe. Short version:

- Make sure `/etc/exports` on `three` uses `async`, not `sync`.
- Remount on Mac with `rsize=1048576,wsize=1048576,nolocks`.
- `sudo mdutil -i off /Volumes/codex_fs` to stop Spotlight from indexing the share.
- For `markdown/`, use `ls` not Finder for large directory listings.

### Garage SIGILL on Pi

Only happens on Garage v2.1.0 and below. Upgrade to v2.2.0, which includes the Pi SIGILL fix (#1217).

### Gateway returns 401 right after enrollment

Most often clock skew between `two` (Pi) and the enrolling device. Install `chrony` on `two` and force a sync:

```bash
sudo apt install -y chrony
sudo chronyc -a 'burst 4/4'
sudo chronyc -a makestep
```

Then re-run `hs cloud invite` and re-enroll.

### `hs upgrade` restarts a watcher on a client that shouldn't run it

You forgot `scribe.local_server: false` in the client's config. Set it:

```yaml
scribe:
  local_server: false
  servers:
    - http://big:7433
```

### `HS_VERSION` baked wrong on a local rc deploy

`build.rs` defaults to `git describe --tags --always`, which picks up the *prior* tag if you haven't tagged yet. For local rc builds, set the version explicitly:

```bash
GITHUB_REF_NAME=v0.0.1-rc.NNN cargo build --release -p hs
```

### Docker / podman noise drowning the real signal

Suppress non-actionable container output on client nodes — they don't need scribe container chatter. Either run with `--quiet`, redirect stderr, or grep out the routine health-check lines.

### Status TUI corrupted during a long-running run

Don't enable raw mode while indicatif progress bars are active. The two terminal-mode systems fight; pick one. (Internal note for contributors — this matters when adding new commands.)

## 12. Updating across the cluster

Before tagging any `rc.*` for distribution to your nodes, the project's release gates from `CLAUDE.md` apply:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test
```

All three must pass. Then tag, build per-host, and roll out in the order above (clients → compute → gateway).

For a local-only rc build (no GitHub release):

```bash
GITHUB_REF_NAME=v0.0.1-rc.NNN cargo build --release -p hs --features ...
GITHUB_REF_NAME=v0.0.1-rc.NNN cargo build --release -p hs-distill --features server,cuda
GITHUB_REF_NAME=v0.0.1-rc.NNN cargo build --release -p hs-gateway
```

Distribute the binaries out of `target/release/` to the right hosts (scp to `~/.local/bin/`), restart services.

## 13. Why the cluster exists

The point isn't the topology. The point is that one person, with a few hundred dollars of Pi hardware and one used GPU box, can run their own research engine across millions of open-access papers — without paying a SaaS, without surrendering their queries to an external service, and without losing access if a vendor folds or hikes prices.

Every role in this guide exists to make that real. The tunnel host so you can reach your library from anywhere. The storage server so your corpus has a home. The GPU compute so conversion and embedding aren't bottlenecks. The DB host so vectors and metadata stay fast. The clients so you actually use the thing.

That's the entire mission: democratizing the conversion of high-quality knowledge into actionable information. The cluster is just what it takes to make that practical at home.
