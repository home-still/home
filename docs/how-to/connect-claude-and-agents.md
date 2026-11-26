# Connect Claude and agents

Give Claude Desktop, Claude Code, OpenCode or any other MCP client access to your library: search it, read documents, cite a passage on its page and add to the library from a chat. An agent uses two MCP servers together. `myelin`, the memory server, is where it searches (`recall`, `investigate`) and keeps what you ask it to remember. `home-still`, the tools server (`hs-mcp`), is where it reads a document, turns a quotation into a boxed page (`cite`), and finds and fetches papers; its tools are listed in the [MCP tools reference](../reference/mcp-tools.md). A client reaches them on your machine, or through the cloud gateway.

## Before you start

- `hs` is installed. The install script puts `hs-mcp` beside it in `~/.local/bin/`.
- For a connection on this machine, `~/.home-still/config.yaml` has the `storage:` and `events:` sections, and the broker and storage are reachable from here. `hs-mcp` does not start without them. See [`events`](../reference/configuration.md#events). The memory server runs on this machine and listens for agents on `127.0.0.1:7463`.
- `~/.home-still/secrets.env` holds `HS_BACKEND_TOKEN`, the same value as on your servers. See [secrets.env](../reference/configuration.md#secretsenv).
- For a remote connection, a gateway runs with `mcp` and `memory` routes and a public URL such as `https://cloud.example.com`. See [Build a home cloud cluster](../tutorials/home-cloud-cluster.md) and [Access your library remotely](access-remotely.md).

## Connect a client on the same machine

1. Write both servers into your clients' configuration. `--client` is `desktop`, `code`, `opencode` or `all` (the default):

   ```sh
   hs mcp install --client code
   ```

   The command finds `hs-mcp` in `~/.local/bin/`, beside `hs`, or in a development build's `target/` directory. When it finds none, it downloads the `hs-mcp` that matches your `hs` version and installs it beside `hs`.

2. Restart the client. The entries it writes for Claude Code look like this:

   ```json
   {
     "mcpServers": {
       "home-still": {
         "command": "/home/<user>/.local/bin/hs-mcp",
         "env": { "HS_BACKEND_TOKEN": "<token>" }
       },
       "myelin": {
         "type": "http",
         "url": "http://127.0.0.1:7463/mcp"
       }
     }
   }
   ```

   The `home-still` entry's `env` copies every value in `secrets.env` into the client's file. A new file is created readable by you alone; an existing file keeps its permissions. Run `hs mcp install` again after you change `secrets.env`. OpenCode keeps its entries under `mcp` in `~/.config/opencode/opencode.json` and uses its own field names.

   Claude Desktop reads only commands from its file, so it gets the `home-still` entry alone, and the command says so. Give it the memory server through the gateway: see [Connect Claude Desktop through the gateway](#connect-claude-desktop-through-the-gateway).

3. To remove both entries from one client or all of them:

   ```sh
   hs mcp uninstall --client code
   ```

If you prefer to edit a client's file yourself, the smallest working `home-still` entry is `{"command": "hs-mcp"}`: the server reads `secrets.env` itself.

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

3. On the gateway host, point the `mcp` route at it in `cloud.gateway.routes`, for example `mcp: http://127.0.0.1:7445`, and the `memory` route at the memory server's agent listener, for example `memory: http://<memory-host>:7463`. See [`cloud.gateway`](../reference/configuration.md#cloudgateway).

## Connect Claude Desktop through the gateway

Claude Desktop connects to a remote MCP server with OAuth 2.1 and PKCE. The gateway implements both ends of it. The two servers are two addresses, `https://cloud.example.com/mcp` and `https://cloud.example.com/memory/mcp`. Do these steps once for each:

1. In Claude Desktop, add the address as a remote MCP server.

2. Claude discovers the gateway's OAuth endpoints, registers itself, and opens your browser to the authorization page. The page names the client and the host it returns to. Check both: anyone can register an OAuth client.

3. On the gateway host, create an enrollment code that grants only that server's scope, `mcp` for `/mcp` and `memory` for `/memory/mcp`:

   ```sh
   hs cloud invite --name claude-desktop --scope mcp
   hs cloud invite --name claude-desktop-memory --scope memory
   ```

   A code is single-use and lasts five minutes, so create each one when you are about to enter it.

4. Enter the code in the browser form and click Authorize. Claude Desktop stores its tokens and refreshes them on its own. The tokens carry exactly the scopes of the code you entered: a token for one server does not open the other.

To cut a connection off later, run `hs cloud revoke --name oauth:<client_id>` on the gateway host. See [Access your library remotely](access-remotely.md#revoke-a-device).

## Point a client at the gateway from a file

`--remote` writes two URL entries that point at the gateway's `/mcp` and `/memory/mcp` paths, in place of the local ones. The gateway URL comes from `--gateway-url`, or else from the credentials `hs cloud enroll` saved:

```sh
hs mcp install --client code --remote --gateway-url https://cloud.example.com
```

The entries hold no credentials; the client authorizes each server through the gateway as above. Claude Desktop takes remote servers in the app and not from its file, so `--remote` writes nothing for it: use the in-app flow in the previous section.

A client that speaks only stdio can use `scripts/home-still-bridge.mjs` from this repository instead. It signs in with the credentials `hs cloud enroll` saved and bridges to the gateway's `/mcp`; run a second copy with `HS_BRIDGE_URL=https://cloud.example.com/memory/mcp` for the memory server. The device must have been enrolled with the `mcp` and `memory` scopes.

## Check it worked

1. Restart the client. It lists a `home-still` server and a `myelin` server, each with its tools.

2. Ask: "Search my library for <a topic>, and show me the page the best passage is on." The agent calls `recall` on `myelin` and `cite` on `home-still`, and answers with the passage and a link that opens the page with the passage boxed. Or ask it to call `status` for the live counts.

3. For a service, confirm that the HTTP server answers, with the same data the client sees:

   ```sh
   HS_MCP_URL=http://localhost:7445/mcp hs status
   ```

4. For a remote connection from an enrolled machine, `hs status` with `HS_MCP_URL` unset goes through the gateway:

   ```sh
   hs cloud status
   hs status
   ```
