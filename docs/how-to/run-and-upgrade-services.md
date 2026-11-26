# Run and upgrade services

Start home-still's services by hand or as services that survive reboots, restart them, upgrade a host to a new release, and see what is running. Ports and unit names are in [services and ports](../reference/services-and-ports.md); every flag is in the [`hs serve`](../reference/cli.md#hs-serve) reference.

## Before you start

- `hs` is installed. The install script also installs `hs-distill-server`, `hs-mcp` and `hs-gateway` in `~/.local/bin/`.
- `~/.home-still/secrets.env` holds `HS_BACKEND_TOKEN`: the scribe, distill and MCP servers refuse to start without it. See [secrets.env](../reference/configuration.md#secretsenv).
- Installing a system service needs `sudo` on Linux. User services and macOS LaunchAgents need none.

## Run a service in the foreground

A foreground run is the way to see a service's log and to check that its configuration works. Stop it with Ctrl+C.

```sh
hs serve scribe
hs serve distill
hs serve mcp
hs serve docs
```

| Service | Default port | Before the first run |
|---|---|---|
| `hs serve scribe` | 7433 | The host has the layout and table models, libpdfium, a VLM backend and `HS_BACKEND_TOKEN`. The command provisions nothing. See [Convert documents](convert-documents.md). |
| `hs serve distill` | 7434 | An NVIDIA GPU with CUDA, and a myelin server and Ollama running. The command provisions neither. See [Index and search your library](index-and-search.md). |
| `hs serve mcp` | 7445 | `storage:` and `events:` in the configuration. See [Connect Claude and agents](connect-claude-and-agents.md). |
| `hs serve docs` | 7446 | `storage:` in the configuration, and libpdfium on the host. See [Open documents with links](open-documents-with-links.md). |

`--port` picks another port:

```sh
hs serve scribe --port 7435
```

`hs serve scribe` stops with an error when `scribe.local_server` is `false`, which marks a machine that only forwards to other scribe servers. A foreground run also refuses to start while the system service of the same name is active, and tells you to stop or uninstall it.

## Run the event watchers

The two watchers turn events into work: one converts each downloaded paper, the other indexes each converted one. Run them in the foreground to see what they do:

```sh
hs serve scribe-watch
hs serve distill-watch
```

## Run distill in the background

For distill, `start` brings up Qdrant and the server and returns; `stop` takes both down:

```sh
hs serve distill start
hs serve distill stop
```

`start` starts Qdrant from `~/.home-still/docker-compose-distill.yml` when that file exists (otherwise Qdrant must already answer), replaces any distill server it started before, waits for `/health`, and starts the indexing daemon. The server's log is `distill-server.log` in the log directory.

## Install a service

An installed service starts at boot and restarts on failure. `--install` installs it; `--uninstall` stops and removes it.

1. Install the servers as system services. Linux asks for your `sudo` password and writes a unit to `/etc/systemd/system/`; macOS writes a LaunchAgent:

   ```sh
   hs serve scribe --install
   hs serve distill --install
   hs serve mcp --install
   hs serve docs --install
   ```

   The service runs the `hs` binary that ran `--install`, on the port you give with `--port`, and reads `~/.home-still/secrets.env`.

2. Install the watchers, and the docs service if you prefer, as user services. They run as you, need no `sudo`, and on Linux keep running after you log out when lingering is enabled (`loginctl enable-linger`):

   ```sh
   hs serve scribe-watch --install
   hs serve distill-watch --install
   hs serve docs --install-user
   ```

3. Remove a service:

   ```sh
   hs serve distill --uninstall
   hs serve scribe-watch --uninstall
   hs serve docs --uninstall
   ```

   `hs serve docs --uninstall` removes the system service and the user service, whichever exists.

`--install` registers one scribe service per host. To run a second server on another port, supervise it yourself; see [Convert documents](convert-documents.md#run-the-scribe-server).

## Look at a service

On Linux, each installed service is a systemd unit. System units are `hs-serve-scribe`, `hs-serve-distill`, `hs-serve-mcp` and `hs-serve-docs`; user units are `hs-scribe-watch-events` and `hs-distill-watch-events`:

```sh
systemctl status hs-serve-distill
journalctl -u hs-serve-distill -f
journalctl --user -u hs-scribe-watch-events -f
```

On macOS the services are LaunchAgents named `com.home-still.<service>`, and their logs are under `~/Library/Logs/home-still/`. Every unit name is in [service units](../reference/services-and-ports.md#service-units).

See the whole pipeline at once with the dashboard, which reads the MCP server's `system_status`:

```sh
HS_MCP_URL=http://localhost:7445/mcp hs status
```

Without `HS_MCP_URL`, `hs status` goes through the gateway with the credentials from `hs cloud enroll`. Each row is described in the [status dashboard](../reference/status-dashboard.md).

## Restart services

After you change `~/.home-still/config.yaml` or `secrets.env`, restart the services on that host:

```sh
hs restart
```

The command finds every systemd unit and LaunchAgent that runs one of this host's home-still binaries, restarts the ones that are active, then restarts the distill indexing daemon and the compose stacks in `~/.home-still/` (Qdrant's `docker-compose-distill.yml`, and the scribe stack's `docker-compose.yml` when `scribe.local_server` is on). A unit that is enabled but stopped is left stopped, with a warning. The command exits non-zero when any restart failed.

## Upgrade a host

Upgrade one host at a time, on the host itself.

1. See whether a newer release exists, without installing it:

   ```sh
   hs upgrade --check --pre
   ```

2. Upgrade. Release candidates are pre-releases, so add `--pre` to include them; `--yes` skips the confirmation:

   ```sh
   hs upgrade --pre --yes
   ```

   The command resolves every download and checksum before it changes anything. Then it updates the managed containers, replaces the binaries, restarts the services that run them, and waits for health.

3. Reinstall the current release over itself, for example after a damaged binary:

   ```sh
   hs upgrade --force
   ```

What the upgrade does, in order:

- **Containers.** For each compose stack in `~/.home-still/` (Qdrant, and the scribe stack when `scribe.local_server` is on), it pulls the images and recreates the containers only when an image changed or the service does not answer its health URL. An unchanged, healthy service keeps running.
- **Binaries.** It replaces `hs-distill-server`, `hs-gateway`, `hs-mcp` and `hs-scribe-server` that are already installed on the host, and replaces `hs` last. A failed run leaves the old `hs` in place, so running it again retries everything.
- **Restarts.** It restarts the units whose program is a binary it replaced, in systemd system scope, systemd user scope and launchd.
- **Health.** It waits for the containers to answer, and for a native distill server to answer on CUDA. A restart or health failure fails the upgrade.

A companion binary that is not installed on the host is not installed by an upgrade. For rolling a whole cluster, see [Operate a cluster](operate-a-cluster.md).

## Check it worked

1. The version matches the release:

   ```sh
   hs --version
   hs upgrade --check --pre
   ```

   The second command reports that you are up to date.

2. The services came back. `hs status` shows each scribe and distill server and Qdrant as healthy:

   ```sh
   HS_MCP_URL=http://localhost:7445/mcp hs status
   ```

3. A service that fails to restart or stay up is listed by `hs restart` with the reason, and in its journal. If an upgrade times out waiting for Qdrant on a large library, Qdrant may still be loading its collections after its container was recreated; see [Troubleshoot](troubleshoot.md), then run `hs upgrade --pre --yes` again.
