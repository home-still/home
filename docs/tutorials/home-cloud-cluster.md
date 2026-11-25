# Build a home-cloud research cluster

In this tutorial you build a personal research engine across a small home
network: a few Raspberry Pis and one Linux box with an NVIDIA GPU. When you
finish, papers you download are converted and indexed on their own, you can
search them from your laptop, and you can reach the library from outside your
home through a Cloudflare tunnel, including from Claude Desktop.

It takes several hours, most of it downloads. To learn the tools on a single
machine first, do [Build your first research library](first-library.md). To
understand why the work is split this way, read the
[architecture](../explanation/architecture.md).

```text
                       Internet
                          │
                   ┌──────▼───────┐
                   │  Cloudflare  │
                   └──────┬───────┘
                          │ tunnel
   ┌──────────────────────▼───────────────────────────────────┐
   │                         home LAN                         │
   │                                                          │
   │  <gateway-host>     <storage-host>      <qdrant-host>    │
   │  cloudflared        NFS export          Qdrant           │
   │  hs-gateway         NATS JetStream                       │
   │                                                          │
   │  <gpu-host>                             <laptop>         │
   │  scribe, distill                        hs, Claude       │
   │  MCP, docs, watchers                                     │
   │                                                          │
   │  <scribe-host> (optional second conversion worker)       │
   └──────────────────────────────────────────────────────────┘
```

## What you need

Each name in angle brackets is a role. Use a host name or address that
resolves on your LAN wherever you see it.

| Role | Hardware that works | Runs |
|---|---|---|
| `<gateway-host>` | Raspberry Pi 4 or 5, 4 GB or more of RAM | `cloudflared`, `hs-gateway` |
| `<storage-host>` | Raspberry Pi 5, 8 GB, a 1 TB or larger USB SSD | The NFS export, NATS |
| `<qdrant-host>` | Raspberry Pi 5, 16 GB, a 512 GB NVMe drive | Qdrant |
| `<gpu-host>` | x86-64 Linux, 32 GB of RAM, an NVIDIA GPU with 8 GB of memory or more | The scribe, distill, MCP and docs servers, the event watchers |
| `<laptop>` | Any computer; the steps use a Mac | `hs`, the inbox watcher, Claude Desktop |
| `<scribe-host>` | An Apple Silicon Mac (optional, step 17) | A second scribe server |

You also need:

- A domain on Cloudflare, for the tunnel. The steps use `cloud.example.com`;
  replace it with your own hostname.
- [Docker](https://docs.docker.com/get-docker/) on `<storage-host>` and
  `<qdrant-host>`.
- [Ollama](https://ollama.com/download) on `<gpu-host>`, for the model that
  reads your PDFs.
- A working NVIDIA driver with CUDA on `<gpu-host>`.
- ONNX Runtime's CUDA provider, `libonnxruntime_providers_cuda.so`, under
  `~/.cache/ort.pyke.io/dfbin/` on `<gpu-host>`. The scribe and distill
  servers load it from there. The release archives do not include it; a build
  of home-still with `--features cuda` puts it there (see
  [contributing](../../CONTRIBUTING.md)).
- `curl`, `tar`, `openssl` and `sha256sum` on every host.

`<lan-cidr>` stands for your LAN's address range, for example
`192.0.2.0/24`. Replace `<user>` with your user name and `<token>` with the
secret you create in step 2. If a host runs `ufw`, the steps show the rule it
needs; run it from `<lan-cidr>` only.

## 1. Install home-still on the gateway host, the GPU host and the laptop

On each of those three hosts:

```sh
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
hs --version
```

The script downloads `hs` and every companion binary that the release
publishes for the host's platform, checks each archive's checksum, and
installs them in `~/.local/bin/`. `hs --version` prints the release after the
program name, for example `0.0.1-rc.388`. If the shell cannot find `hs`, add
`~/.local/bin` to your `PATH` as the script tells you.

Every host gets `hs`, `hs-gateway` and `hs-mcp`. Only `<gpu-host>` gets
`hs-distill-server`, because the release publishes it for x86-64 Linux only.
The scribe server is a separate download, which you fetch in step 8.

## 2. Create the shared secret

The scribe, distill and MCP servers and the gateway refuse to start, and
every client is refused, unless they all hold the same secret. Create it once
on any host:

```sh
openssl rand -hex 32
```

You see 64 hexadecimal characters. On `<gateway-host>`, `<gpu-host>` and
`<laptop>`, save the value:

```sh
mkdir -p ~/.home-still
echo 'HS_BACKEND_TOKEN=<token>' >> ~/.home-still/secrets.env
chmod 600 ~/.home-still/secrets.env
```

Every home-still program reads `secrets.env` when it starts. Treat the file as
the cluster's root credential: the value travels unencrypted between the hosts
on your LAN, and anyone who holds it can call every backend directly,
bypassing the gateway's scopes. Keep the backend addresses in your
configuration on your own network.

## 3. Export the storage on the storage host

`<storage-host>` owns the only copy of your papers, markdown and catalog. This
step assumes its USB SSD is mounted at `/mnt/share`. On `<storage-host>`:

```sh
sudo apt install -y nfs-kernel-server
sudo mkdir -p /mnt/share/home-still/{papers,markdown,catalog,logs}
sudo mkdir -p /mnt/share/home-still/papers/manually_downloaded
sudo chown -R 1000:1000 /mnt/share/home-still
```

Never run `chown -R` from a parent directory that has other mounts inside it.
Run it only inside the directory you own.

Add this line to `/etc/exports`:

```text
/mnt/share/home-still   <lan-cidr>(rw,async,no_subtree_check,all_squash,anonuid=1000,anongid=1000)
```

Keep `async`. With `sync`, every write waits for the disk, and a USB SSD
crawls. Apply the export:

```sh
sudo exportfs -ra
sudo systemctl enable --now nfs-server
showmount -e localhost
```

`showmount` lists `/mnt/share/home-still` and your `<lan-cidr>`. If the host
runs `ufw`, allow NFS from the LAN:

```sh
sudo ufw allow from <lan-cidr> to any port 2049 proto tcp
```

## 4. Start NATS on the storage host

Every stored document announces itself on an event bus, and the conversion and
indexing workers listen there. home-still uses NATS with JetStream, and no
worker or download command runs without it. On `<storage-host>`:

```sh
mkdir -p ~/nats-data
docker run -d --name nats --restart unless-stopped \
  -p 4222:4222 -v ~/nats-data:/data nats -js -sd /data
docker logs nats
```

The log ends with the server ready and JetStream started. The workers create
their streams and durable consumers the first time they connect; you do
nothing more on the broker.

## 5. Start Qdrant on the database host

On `<qdrant-host>`:

```sh
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

`healthz` answers with Qdrant's health text and `collections` lists none yet.

Qdrant stores one vector per chunk of text. Each vector has 1024 numbers, so
it takes 4 KiB as 32-bit floats before Qdrant adds its search graph and the
chunk's text. The distill server creates its collections with the vectors and
payloads on disk, so plan disk space at more than 4 KiB per chunk and leave
RAM for the operating system to cache the files it reads most.

## 6. Prepare the GPU host: driver and storage

On `<gpu-host>`, install the NVIDIA driver, CUDA and the NFS client. The
commands below are for Arch Linux and its derivatives; on Ubuntu or Debian,
follow NVIDIA's CUDA installation guide for your release and install
`nfs-common`. Do not mix distribution packages with NVIDIA's `.run` installer.

```sh
sudo pacman -S nvidia nvidia-utils cuda cudnn nfs-utils
nvidia-smi
nvcc --version
```

`nvidia-smi` lists your GPU and its memory, and `nvcc` prints the CUDA
version. Mount the storage:

```sh
sudo mkdir -p /mnt/share/home-still
echo '<storage-host>:/mnt/share/home-still  /mnt/share/home-still  nfs4  rw,async,vers=4,_netdev  0  0' \
  | sudo tee -a /etc/fstab
sudo mount /mnt/share/home-still
ls /mnt/share/home-still
```

`ls` lists `catalog`, `logs`, `markdown` and `papers`.

## 7. Configure home-still on the GPU host

Write `~/.home-still/config.yaml` on `<gpu-host>`:

```yaml
home:
  project_dir: /mnt/share/home-still

storage:
  backend: local
  local:
    root: /mnt/share/home-still

events:
  backend: nats
  nats:
    url: nats://<storage-host>:4222

paper:
  download:
    unpaywall_email: you@example.com

scribe:
  servers:
    - http://localhost:7433

distill:
  servers:
    - http://localhost:7434

distill_server:
  qdrant_url: http://<qdrant-host>:6334
```

Every program on the host reads this file. `events:` is required, `storage:`
points the whole pipeline at the NFS mount, and `scribe.servers` and
`distill.servers` name the servers the watchers send work to. Replace the
Unpaywall address with yours; Unpaywall finds more open copies of papers.

To check that the file loads:

```sh
hs config show
```

You see the effective paper configuration as YAML, with your Unpaywall email.
Every key is described in the [configuration reference](../reference/configuration.md).

## 8. Set up conversion on the GPU host

The scribe server needs the model that reads text, two small layout models,
the PDF library pdfium, and the server binary.

Pull the reading model into Ollama:

```sh
ollama pull glm-ocr:latest
ollama list
```

`ollama list` shows `glm-ocr:latest`.

Put the layout model `pp-doclayoutv3.onnx` (PP-DocLayout-V3, from
[PaddleX](https://github.com/PaddlePaddle/PaddleX)) and the table model
`slanet-plus.onnx` (SLANet-Plus, from
[PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR)) in
`~/.home-still/models/`, as ONNX files with exactly those names:

```sh
mkdir -p ~/.home-still/models
ls ~/.home-still/models
```

`ls` lists `pp-doclayoutv3.onnx` and `slanet-plus.onnx`. The server refuses to
start without them.

Install pdfium and the scribe server from their releases:

```sh
cd "$(mktemp -d)"
curl -fLO "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7749/pdfium-linux-x64.tgz"
tar -xzf pdfium-linux-x64.tgz
mkdir -p ~/.local/lib && cp lib/libpdfium.so ~/.local/lib/

VERSION="v$(hs --version | cut -d' ' -f2)"
ARCHIVE="hs-scribe-server-$VERSION-x86_64-unknown-linux-gnu.tar.gz"
curl -fLO "https://github.com/home-still/home/releases/download/$VERSION/$ARCHIVE"
curl -fLO "https://github.com/home-still/home/releases/download/$VERSION/$ARCHIVE.sha256"
sha256sum -c "$ARCHIVE.sha256"
tar -xzf "$ARCHIVE" -C ~/.local/bin hs-scribe-server
```

`sha256sum -c` prints `OK`. Run the scribe server as a supervised service:

```sh
hs serve scribe --install
curl -fsS http://localhost:7433/health
```

`hs serve scribe --install` writes the systemd unit `hs-serve-scribe.service`,
enables it and starts it; it asks for your `sudo` password. `/health` answers
with JSON whose `status` is `ok`. If it does not, read the unit's log with
`journalctl -u hs-serve-scribe -f` and see
[Troubleshoot](../how-to/troubleshoot.md#a-scribe-or-distill-server-does-not-start).

## 9. Start the distill server

The distill server turns markdown into vectors on the GPU and writes them to
Qdrant. It runs only on CUDA: there is no build and no setting that makes it
fall back to the CPU.

First check that the server can reach Qdrant:

```sh
hs distill init
```

`hs distill init` prints that Qdrant is already reachable at
`http://<qdrant-host>:6333` and that it found the `hs-distill-server` binary.
Then install the server:

```sh
hs serve distill --install
journalctl -u hs-serve-distill -f
```

The first start downloads the embedding model, which takes several minutes.
When the log reports that CUDA is verified and the model is on the GPU, press
Ctrl+C and check the server:

```sh
curl -fsS http://localhost:7434/health
hs distill status
```

`/health` answers with HTTP 200, and `hs distill status` shows the collection
in Qdrant and the compute device `cuda`. If the server exits instead, see
[Troubleshoot](../how-to/troubleshoot.md#a-scribe-or-distill-server-does-not-start).

## 10. Start the MCP and docs servers and open the ports

The MCP server gives assistants and `hs status` their view of the pipeline.
The docs server serves documents over signed, expiring links. Add the link
settings to `~/.home-still/config.yaml` on `<gpu-host>`:

```yaml
docs:
  lan_base_url: http://<gpu-host>:7446
  remote_base_url: https://cloud.example.com
```

Install both services:

```sh
hs serve mcp --install
hs serve docs --install
curl -fsS http://localhost:7446/health
```

The docs server's `/health` answers with HTTP 200. If `<gpu-host>` runs `ufw`,
open the four server ports to the LAN. A firewall that drops a port you did not
open shows up as a seven-second timeout from every other host, while the
service answers normally on its own host.

```sh
sudo ufw allow from <lan-cidr> to any port 7433,7434,7445,7446 proto tcp
```

Check the pipeline's view of itself:

```sh
HS_MCP_URL=http://localhost:7445/mcp hs status
```

You see the dashboard with the scribe server and the distill server listed
under Services and the counts at zero. The rows are described in the
[status dashboard reference](../reference/status-dashboard.md).

## 11. Start the watchers and run a paper through

Two event watchers connect the pipeline: one turns each new source document
into markdown, and one indexes each new markdown document. Run them on
`<gpu-host>` only; they read and write the storage. They are user services, so
keep them running after you log out first:

```sh
sudo loginctl enable-linger "$USER"
hs serve scribe-watch --install
hs serve distill-watch --install
systemctl --user status hs-scribe-watch-events hs-distill-watch-events
```

Both units show `active (running)`. Each watcher shares one durable JetStream
consumer with any other watcher of its kind, and the broker redelivers an event
until a watcher finishes it.

Now send a paper through:

```sh
hs paper download --doi 10.48550/arXiv.1706.03762
HS_MCP_URL=http://localhost:7445/mcp hs status
```

The PDF lands under `/mnt/share/home-still/papers/`. Run `hs status` again every
minute or so: the Documents count reaches 1, then Markdown reaches 1 when
conversion finishes, then Embedded reaches 1 when indexing finishes. Then
search:

```sh
hs distill search "self-attention without recurrence" --limit 3
```

You see ranked passages from the paper. Sharing one GPU between the three
programs is described in [Sharing one GPU](../explanation/architecture.md#sharing-one-gpu).

## 12. Set up the tunnel on the gateway host

`<gateway-host>` is the only machine reachable from the internet, and it
reaches the internet only outward: `cloudflared` keeps one connection open to
Cloudflare, so your router opens no port. On `<gateway-host>`, install
`cloudflared` (this is the package for a 64-bit Raspberry Pi OS):

```sh
curl -LO https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-arm64.deb
sudo dpkg -i cloudflared-linux-arm64.deb
cloudflared --version
```

Authorize it and create the tunnel:

```sh
cloudflared tunnel login
cloudflared tunnel create home-still
```

`login` prints a link; open it in a browser and pick your domain. `create`
prints the tunnel's UUID and writes its credentials to
`~/.cloudflared/<TUNNEL_ID>.json`. Keep that file secret. Write
`~/.cloudflared/config.yml`, replacing `<TUNNEL_ID>`, `<user>` and the
hostname:

```yaml
tunnel: <TUNNEL_ID>
credentials-file: /home/<user>/.cloudflared/<TUNNEL_ID>.json

ingress:
  - hostname: cloud.example.com
    service: http://127.0.0.1:7440
    originRequest:
      connectTimeout: 30s
      keepAliveTimeout: 600s
  - service: http_status:404
```

Point the hostname at the tunnel and run the tunnel as a service:

```sh
cloudflared tunnel route dns home-still cloud.example.com
sudo mkdir -p /etc/cloudflared
sudo cp ~/.cloudflared/config.yml /etc/cloudflared/config.yml
sudo cloudflared service install
sudo systemctl status cloudflared
```

`systemctl` shows `cloudflared` active.

## 13. Run the gateway

The gateway checks each caller's token and forwards allowed requests to the
servers on `<gpu-host>`. Create its signing secret and admin key on
`<gateway-host>`:

```sh
hs cloud init
```

`hs cloud init` writes `~/.home-still/cloud-secret.key` and
`~/.home-still/cloud-admin.key`, both mode `0600`. Then add the gateway to
`~/.home-still/config.yaml`:

```yaml
cloud:
  gateway:
    listen: 127.0.0.1:7440
    routes:
      scribe: http://<gpu-host>:7433
      distill: http://<gpu-host>:7434
      mcp: http://<gpu-host>:7445
      docs: http://<gpu-host>:7446
```

The gateway refuses to start unless `routes` names a backend for each of
`scribe`, `distill`, `mcp` and `docs`. The `docs` route is what makes signed
document links work from outside your LAN. The other keys, and the gateway's
endpoints, are in the [configuration reference](../reference/configuration.md#cloudgateway)
and the [services reference](../reference/services-and-ports.md#gateway-endpoints).

The gateway's public address is not in the configuration; you pass it with
`--gateway-url`, and it must start with `https://`. Run the gateway as a
service:

```sh
sudo tee /etc/systemd/system/hs-gateway.service > /dev/null <<'EOF'
[Unit]
Description=home-still cloud gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=<user>
ExecStart=/home/<user>/.local/bin/hs-gateway --gateway-url https://cloud.example.com
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now hs-gateway
sudo journalctl -u hs-gateway -f
```

The gateway reads `HS_BACKEND_TOKEN` from `~/.home-still/secrets.env`, and
replaces each caller's token with it when it forwards a request. At startup it
probes every backend from its own host and logs a warning naming any it cannot
reach. Press Ctrl+C, then check both addresses:

```sh
curl -fsS http://127.0.0.1:7440/health
curl -fsS https://cloud.example.com/health
```

Both answer `ok`. If the log warned that a backend is unreachable, see
[Troubleshoot](../how-to/troubleshoot.md#the-gateway-reports-an-unreachable-backend).

## 14. Connect the laptop

On `<laptop>`, mount the storage. macOS needs large read and write sizes, or
Finder hangs on the shared directories:

```sh
sudo mkdir -p /Volumes/share
sudo mount_nfs -o resvport,rw,rsize=1048576,wsize=1048576,nolocks \
    <storage-host>:/mnt/share/home-still /Volumes/share
sudo mdutil -i off /Volumes/share
ls /Volumes/share
```

`ls` lists `catalog`, `logs`, `markdown` and `papers`. The mount does not
survive a restart; repeat the `mount_nfs` line after rebooting. Write
`~/.home-still/config.yaml`:

```yaml
home:
  project_dir: /Volumes/share

storage:
  backend: local
  local:
    root: /Volumes/share

events:
  backend: nats
  nats:
    url: nats://<storage-host>:4222

scribe:
  local_server: false
  servers:
    - http://<gpu-host>:7433

distill:
  servers:
    - http://<gpu-host>:7434
```

`scribe.local_server: false` tells `hs` that this machine only uses scribe
servers and never runs one. Then enroll the laptop with the gateway so it can
reach the cluster from outside. On `<gateway-host>`:

```sh
hs cloud invite --name laptop
```

The command prints a one-time code that works for five minutes. On `<laptop>`:

```sh
hs cloud enroll --gateway https://cloud.example.com
hs cloud status
```

Paste the code when `hs cloud enroll` asks. `hs cloud status` shows that the
laptop is enrolled with `https://cloud.example.com`. Search from home, directly
on the LAN:

```sh
hs distill search "self-attention without recurrence" --limit 3
```

From outside your home network, send the same search through the gateway
instead:

```sh
hs distill search "self-attention without recurrence" --limit 3 --server https://cloud.example.com
```

Both print the same passages. An address that starts with `https://` makes
`hs` attach the credentials you enrolled with. Only the search and MCP paths
work away from home: downloads and the inbox need the storage mount and the
event bus on your LAN.

## 15. Connect Claude Desktop

Claude Desktop reaches the MCP server through the gateway, using OAuth. On
`<gateway-host>`, create a code that grants only the MCP scope:

```sh
hs cloud invite --name claude-desktop --scope mcp
```

Then in Claude Desktop:

1. Add `https://cloud.example.com/mcp` as a remote MCP server.
2. Claude opens your browser on the gateway's authorization page.
3. Paste the code and select Authorize.

Claude Desktop stores its tokens and refreshes them with its refresh token. In
a new chat, ask Claude to list the home-still tools: you see the paper, scribe,
distill and status tools. They are listed in the [MCP tools reference](../reference/mcp-tools.md).

## 16. Watch the inbox from the laptop

The inbox picks up files you download by hand. Run its watcher on one client
that can write to the storage, and on no other. Two copies sweeping the same
prefix race each other. On `<laptop>`:

```sh
hs scribe inbox install
hs scribe inbox status
```

`install` registers a LaunchAgent on macOS, or a user systemd unit on Linux,
and starts it; `status` reports it running. The watcher polls
`papers/manually_downloaded/` every `scribe.inbox_poll_interval_secs` seconds
(30 by default). Drop a PDF in:

```sh
cp ~/Downloads/paper.pdf /Volumes/share/papers/manually_downloaded/
hs status
```

Within the poll interval the Inbox row shows the file waiting, then the
watcher moves the file to its place under `papers/` and announces it, and the
pipeline converts and indexes it as in step 11. The watcher's log is in
`~/Library/Logs/home-still-scribe-inbox.log` on macOS, or in
`journalctl --user -u home-still-scribe-inbox.service` on Linux. Remove the
watcher with `hs scribe inbox uninstall`.

## 17. Add a second scribe worker

A second scribe host adds conversion capacity. It is a plain HTTP worker: it
runs only the scribe server, and the watcher on `<gpu-host>` sends it
documents and stores the results. It needs no storage mount, no event bus and
no watcher, so it can sit on a flaky wifi link. This step uses an Apple
Silicon Mac as `<scribe-host>`.

On `<scribe-host>`, install `hs` (step 1), save the shared secret in
`~/.home-still/secrets.env` (step 2) and install
[Ollama](https://ollama.com/download). Then pull the model and install the
layout models, pdfium and the server:

```sh
ollama pull glm-ocr:latest
mkdir -p ~/.home-still/models
ls ~/.home-still/models
```

`ls` lists `pp-doclayoutv3.onnx` and `slanet-plus.onnx`; put them there as in
step 8. Then:

```sh
cd "$(mktemp -d)"
curl -fLO "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7749/pdfium-mac-arm64.tgz"
tar -xzf pdfium-mac-arm64.tgz
mkdir -p ~/.local/lib && cp lib/libpdfium.dylib ~/.local/lib/

VERSION="v$(hs --version | cut -d' ' -f2)"
ARCHIVE="hs-scribe-server-$VERSION-aarch64-apple-darwin.tar.gz"
curl -fLO "https://github.com/home-still/home/releases/download/$VERSION/$ARCHIVE"
curl -fLO "https://github.com/home-still/home/releases/download/$VERSION/$ARCHIVE.sha256"
shasum -a 256 -c "$ARCHIVE.sha256"
tar -xzf "$ARCHIVE" -C ~/.local/bin hs-scribe-server

hs serve scribe --install
curl -fsS http://localhost:7433/health
```

`shasum` prints `OK`, and `hs serve scribe --install` registers the LaunchAgent
`com.home-still.scribe` and starts it. `/health` answers with JSON whose
`status` is `ok`.

Now add the worker to the pool. On `<gpu-host>`, list it second in
`~/.home-still/config.yaml`:

```yaml
scribe:
  servers:
    - http://localhost:7433
    - http://<scribe-host>:7433
```

Restart the scribe watcher so it reads the new list, then look at the services:

```sh
systemctl --user restart hs-scribe-watch-events.service
HS_MCP_URL=http://localhost:7445/mcp hs status
```

The Services rows list both scribe servers. Two entries with the same backend
form one tier, and the watcher sends each document to the least busy server in
the tier; see [the scribe tier chain](../explanation/architecture.md#the-scribe-tier-chain).

Do not run the watchers or the distill server on `<scribe-host>`. A watcher
needs to write to the storage and the catalog, which this host does not
reach, and the pipeline has one distill host.

## Where to go next

- [Operate a cluster](../how-to/operate-a-cluster.md): verify, upgrade, add a
  GPU node, take a node out and recover Qdrant.
- [Troubleshoot](../how-to/troubleshoot.md): a symptom, its cause and its fix.
- [Access remotely](../how-to/access-remotely.md): enroll more devices and
  revoke them.
- [Open documents with links](../how-to/open-documents-with-links.md): the
  signed links the docs server serves.
- [Connect Claude and agents](../how-to/connect-claude-and-agents.md): other
  clients.
- [Services, ports and endpoints](../reference/services-and-ports.md): every
  port and unit name.
