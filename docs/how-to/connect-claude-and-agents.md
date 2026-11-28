# Connect Claude and agents

Give Claude Desktop, Claude Code, OpenCode or any other MCP client access to your library: search it, read documents, cite a passage on its page and add to the library from a chat. An agent uses one MCP server, `home-still` (`hs-mcp`). It serves the library's tools: reading a document, turning a quotation into a boxed page (`cite`), finding and fetching papers. It also serves the memory and search tools (`recall`, `investigate`, `remember`, `forget`, and the rest of myelin's agent tools), which it forwards to myelin on the memory host's loopback interface and answers with myelin's result unchanged. myelin's agent surface is not exposed to clients. The tools are listed in the [MCP tools reference](../reference/mcp-tools.md). A client reaches the server on your machine, or through the cloud gateway.

## Before you start

- `hs` is installed. The install script puts `hs-mcp` beside it in `~/.local/bin/`.
- For a connection on this machine, `~/.home-still/config.yaml` has the `storage:` and `events:` sections, and the broker and storage are reachable from here. `hs-mcp` does not start without them. See [`events`](../reference/configuration.md#events). `hs-mcp` reaches myelin on this machine, at `127.0.0.1:7463` unless `mcp.myelin_url` says otherwise.
- `~/.home-still/secrets.env` holds `HS_BACKEND_TOKEN`, the same value as on your servers. See [secrets.env](../reference/configuration.md#secretsenv).
- For a remote connection, a gateway runs with an `mcp` route and a public URL such as `https://cloud.example.com`. See [Build a home cloud cluster](../tutorials/home-cloud-cluster.md) and [Access your library remotely](access-remotely.md).

## Connect a client on the same machine

1. Write the server into your clients' configuration. `--client` is `desktop`, `code`, `opencode` or `all` (the default):

   ```sh
   hs mcp install --client code
   ```

   The command finds `hs-mcp` in `~/.local/bin/`, beside `hs`, or in a development build's `target/` directory. When it finds none, it downloads the `hs-mcp` that matches your `hs` version and installs it beside `hs`.

2. Restart the client. The entry it writes for Claude Code looks like this:

   ```json
   {
     "mcpServers": {
       "home-still": {
         "command": "/home/<user>/.local/bin/hs-mcp",
         "env": { "HS_BACKEND_TOKEN": "<token>" }
       }
     }
   }
   ```

   The entry's `env` copies every value in `secrets.env` into the client's file. A new file is created readable by you alone; an existing file keeps its permissions. Run `hs mcp install` again after you change `secrets.env`. OpenCode keeps its entry under `mcp` in `~/.config/opencode/opencode.json` and uses its own field names.

3. To remove the entry from one client or all of them:

   ```sh
   hs mcp uninstall --client code
   ```

   The command removes the `home-still` entry and no other.

If you prefer to edit a client's file yourself, the smallest working entry is `{"command": "hs-mcp"}`: the server reads `secrets.env` itself.

## Run the MCP server as a service

A service speaks HTTP, so the gateway, `hs status` and remote clients can reach it. It needs the same `storage:` and `events:` sections, and refuses to start without `HS_BACKEND_TOKEN`. Every request must carry `Authorization: Bearer <token>`.

1. Run it in the foreground to see its log:

   ```sh
   hs serve mcp
   ```

   It listens on port 7445. `--port` changes it:

   ```sh
   hs serve mcp --port 7445
   ```

2. Install it as a system service that starts at boot and restarts on failure (it asks for your `sudo` password), or remove it:

   ```sh
   hs serve mcp --install
   hs serve mcp --uninstall
   ```

3. On the gateway host, point the `mcp` route at it in `cloud.gateway.routes`, for example `mcp: http://127.0.0.1:7445`. See [`cloud.gateway`](../reference/configuration.md#cloudgateway).

## Connect Claude Desktop through the gateway

Claude Desktop connects to a remote MCP server with OAuth 2.1 and PKCE. The gateway implements both ends of it. The server is at `https://cloud.example.com/mcp`.

1. In Claude Desktop, add that address as a remote MCP server.

2. Claude discovers the gateway's OAuth endpoints, registers itself, and opens your browser to the authorization page. The page names the client and the host it returns to. Check both: anyone can register an OAuth client.

3. On the gateway host, create an enrollment code that grants the `mcp` scope:

   ```sh
   hs cloud invite --name claude-desktop --scope mcp
   ```

   A code is single-use and lasts five minutes, so create it when you are about to enter it.

4. Enter the code in the browser form and click Authorize. Claude Desktop stores its tokens and refreshes them on its own. The tokens carry exactly the scopes of the code you entered.

To cut a connection off later, run `hs cloud revoke --name oauth:<client_id>` on the gateway host. See [Access your library remotely](access-remotely.md#revoke-a-device).

## Point a client at the gateway from a file

`--remote` writes a URL entry that points at the gateway's `/mcp` path, in place of the local one. The gateway URL comes from `--gateway-url`, or else from the credentials `hs cloud enroll` saved:

```sh
hs mcp install --client code --remote --gateway-url https://cloud.example.com
```

The entry holds no credentials; the client authorizes the server through the gateway as above. Claude Desktop takes remote servers in the app and not from its file, so `--remote` writes nothing for it: use the in-app flow in the previous section.

A client that speaks only stdio can use `scripts/home-still-bridge.mjs` from this repository instead. It signs in with the credentials `hs cloud enroll` saved and bridges to the gateway's `/mcp`. The device must have been enrolled with the `mcp` scope.

## Check it worked

1. Restart the client. It lists a `home-still` server with its tools.

2. Ask: "Search my library for <a topic>, and show me the page the best passage is on." The agent calls `recall` and `cite` on `home-still`, and answers with the passage and a link that opens the page with the passage boxed. Or ask it to call `status` for the live counts.

3. For a service, confirm that the HTTP server answers, with the same data the client sees:

   ```sh
   HS_MCP_URL=http://localhost:7445/mcp hs status
   ```

4. For a remote connection from an enrolled machine, `hs status` with `HS_MCP_URL` unset goes through the gateway:

   ```sh
   hs cloud status
   hs status
   ```
