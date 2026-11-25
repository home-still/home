# Access your library remotely

Enroll a device with your cloud gateway so `hs` and Claude can reach your library from anywhere, revoke a device that you lose, and rotate the gateway's signing secret. The gateway is the single, authenticated entry point for off-network clients. How its trust model works is explained in [architecture](../explanation/architecture.md).

## Before you start

- The gateway is running behind a tunnel at a public `https://` URL, such as `https://cloud.example.com`, and `cloud.gateway.routes` lists a backend for each service. [Build a home cloud cluster](../tutorials/home-cloud-cluster.md) sets this up, gateway host included.
- You can run commands on the gateway host: `hs cloud invite` and `hs cloud revoke` read the admin key that sits beside the gateway's signing secret, so they work only there. They call the gateway at `cloud.gateway.listen` from its own config.
- `HS_BACKEND_TOKEN` has the same value on the gateway and on every backend host. See [secrets.env](../reference/configuration.md#secretsenv).

## Enroll a device

1. On the gateway host, create an enrollment code for the device:

   ```sh
   hs cloud invite --name laptop
   ```

   The command prints a one-time code, the scopes it grants and when it expires: five minutes after creation, and good for one enrollment.

2. Grant fewer scopes when the device needs less. `--scope` takes `scribe`, `distill` or `mcp`, and repeats; without it the code grants all three. A viewer that only uses Claude needs `mcp`:

   ```sh
   hs cloud invite --name viewer --scope mcp
   hs cloud invite --name worker --scope scribe --scope distill
   ```

   The name and scopes are fixed by the invite. The enrolling device cannot choose its own.

3. On the device, enroll with the gateway's public URL and enter the code at the prompt:

   ```sh
   hs cloud enroll --gateway https://cloud.example.com
   ```

   The URL must be a bare `https://` origin. The device saves a refresh token in `~/.home-still/cloud-token`, readable by you alone.

4. Point the device's servers at the gateway. Any server URL that starts with `https://` is called through the gateway with the device's credentials. In `~/.home-still/config.yaml` on the device:

   ```yaml
   scribe:
     servers:
       - https://cloud.example.com
   distill:
     servers:
       - https://cloud.example.com
   ```

   With `HS_MCP_URL` unset, `hs status` also reaches the MCP server through the gateway. See [`scribe`](../reference/configuration.md#scribe) and [`distill`](../reference/configuration.md#distill).

To connect Claude Desktop, follow [Connect Claude and agents](connect-claude-and-agents.md#connect-claude-desktop-through-the-gateway).

## Check a device's connection

On the device:

```sh
hs cloud status
```

It prints the gateway, the device name and `Connection: OK (token refreshed)`. It exits non-zero when the connection is broken, so scripts can rely on it.

For scripts and CI, print a fresh access token. Only the token goes to standard output, so `$(hs cloud token)` captures it. It lasts 4 hours unless `cloud.gateway.token_ttl_secs` says otherwise:

```sh
hs cloud token
```

Send it to a gateway path as `Authorization: Bearer <token>`. The paths and the scope each needs are in [services and ports](../reference/services-and-ports.md#proxied-paths).

## Revoke a device

On the gateway host, revoke a device by the name you gave it, or an OAuth client by `oauth:<client_id>`:

```sh
hs cloud revoke --name laptop
hs cloud revoke --name oauth:hs-abc123
```

Every token issued to that name stops working at once, access and refresh alike, and the revocation survives a gateway restart (it is stored in `cloud-revoked.json` beside the signing secret). To let the device back in, create a new invite and enroll it again.

Document links bound to the revoked name stop working as well, once the docs service has re-read the revocation list. It does that at start-up, so restart it on the host that runs it with `hs restart`. See [Open documents with links](open-documents-with-links.md).

## Rotate the signing secret

Rotate the secret that signs tokens without logging every device out. The old secret stays valid for a grace period.

1. On the gateway host, move the current secret aside:

   ```sh
   cd ~/.home-still
   mv cloud-secret.key cloud-secret.key.prev
   ```

2. Tell the gateway to still accept tokens signed with it. In `~/.home-still/config.yaml`, under `cloud.gateway`:

   ```yaml
   cloud:
     gateway:
       previous_secret_path: /home/<user>/.home-still/cloud-secret.key.prev
   ```

3. Restart the gateway. It creates a new `cloud-secret.key` and signs new tokens with it. Use the name of the service you installed for it:

   ```sh
   sudo systemctl restart hs-gateway
   ```

4. After the refresh-token lifetime has passed, 7 days by default, remove `previous_secret_path` from the config, restart the gateway again and delete `cloud-secret.key.prev`.

Removing `previous_secret_path` earlier signs everyone out: every token signed with the old secret stops working and every device must enroll again. The gateway refuses to start when `previous_secret_path` is set but the file is missing or too short. The admin key and the document-link key are separate files, and rotation does not touch them. See [`cloud.gateway`](../reference/configuration.md#cloudgateway).

## Check it worked

1. A newly enrolled device connects:

   ```sh
   hs cloud status
   ```

   It prints `Connection: OK (token refreshed)`.

2. After a revocation, the same command on the revoked device exits non-zero with `Connection failed`.

3. After a rotation, devices keep working through the grace period without enrolling again, and `hs cloud status` on one still prints `OK`.

If a freshly enrolled device is refused with `401`, check that the clocks of the gateway host and the device agree; see [Troubleshoot](troubleshoot.md).
