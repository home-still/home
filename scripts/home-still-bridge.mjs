#!/usr/bin/env node
// stdio<->HTTPS bridge for home-still MCP behind the cloud gateway.
// Reads newline-delimited JSON-RPC from stdin, POSTs each message to the
// gateway's /mcp endpoint with a bearer access token (refreshed on demand
// from ~/.home-still/cloud-token), parses the SSE response, and writes each
// non-empty `data:` payload as one stdout line.
//
// Invariants (each one fixes a hang seen with omp's 900 s tool timeout):
//  1. The MCP server (rmcp) requires `initialize`, then `notifications/
//     initialized`, then everything else, strictly in order. stdio clients
//     write these back-to-back, so handshake messages are POSTed one at a
//     time and every other message waits for the handshake to finish.
//     Post-handshake requests run concurrently.
//  2. Every request (message with an id) gets exactly one stdout reply. A
//     transport failure, non-2xx status, or an SSE stream that ends without
//     a matching response becomes a JSON-RPC error, never silence.
//  3. A 404 means the server dropped the session (idle timeout, restart).
//     The bridge re-runs the handshake once with the client's original
//     `initialize` payload and retries the request once.
//
// Endpoint is derived from the enrolled gateway_url in the credential file;
// override with HS_BRIDGE_URL. Fails loudly on missing credentials or a
// failed token refresh.

import { stdin, stdout, stderr } from "node:process";
import readline from "node:readline";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const CRED_PATH = path.join(os.homedir(), ".home-still", "cloud-token");

function loadCredentials() {
  let raw;
  try {
    raw = fs.readFileSync(CRED_PATH, "utf8");
  } catch (e) {
    throw new Error(`cannot read ${CRED_PATH}: ${e.message}`);
  }
  const creds = JSON.parse(raw);
  if (!creds.refresh_token || !creds.gateway_url) {
    throw new Error(`credentials at ${CRED_PATH} missing refresh_token/gateway_url`);
  }
  return creds;
}

const creds = loadCredentials();
const gateway = creds.gateway_url.replace(/\/+$/, "");
const ENDPOINT = process.env.HS_BRIDGE_URL || `${gateway}/mcp`;

let accessToken = null;
let sessionId = null;
let initPayload = null; // the client's own `initialize` line, replayed on 404
let reinit = null; // in-flight re-handshake shared by concurrent 404s
let inflight = 0;
let stdinClosed = false;

function logErr(...a) { stderr.write("[bridge] " + a.join(" ") + "\n"); }
function maybeExit() { if (stdinClosed && inflight === 0) process.exit(0); }

function replyError(id, message) {
  stdout.write(JSON.stringify({ jsonrpc: "2.0", id, error: { code: -32000, message: `home-still bridge: ${message}` } }) + "\n");
}

async function refreshAccessToken() {
  const res = await fetch(`${gateway}/cloud/refresh`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "Authorization": `Bearer ${creds.refresh_token}`,
    },
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`token refresh failed (${res.status}): ${body}`);
  }
  const data = await res.json();
  if (!data.access_token) {
    throw new Error("token refresh response missing access_token");
  }
  accessToken = data.access_token;
}

function send(payload) {
  return fetch(ENDPOINT, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "Accept": "application/json, text/event-stream",
      "Authorization": `Bearer ${accessToken}`,
      ...(sessionId ? { "mcp-session-id": sessionId } : {}),
    },
    body: payload,
  });
}

// POST once, refreshing an expired access token once. Throws on transport
// failure or a non-2xx status (error carries `.status`).
async function post(payload) {
  let res = await send(payload);
  if (res.status === 401) {
    await refreshAccessToken();
    res = await send(payload);
  }
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    const err = new Error(`gateway error ${res.status}: ${body.slice(0, 300)}`);
    err.status = res.status;
    throw err;
  }
  const sid = res.headers.get("mcp-session-id");
  if (sid) sessionId = sid;
  return res;
}

// Parse an SSE body; returns the set of response ids written to stdout.
function relay(buf) {
  const seen = new Set();
  for (const event of buf.split(/\r?\n\r?\n/)) {
    if (!event.trim()) continue;
    for (const line of event.split(/\r?\n/)) {
      let data = null;
      if (line.startsWith("data: ")) data = line.slice(6);
      else if (line.startsWith("data:")) data = line.slice(5);
      if (data === null) continue;
      data = data.trim();
      if (!data) continue;
      stdout.write(data + "\n");
      try {
        const o = JSON.parse(data);
        if (o && o.id !== undefined && o.id !== null && ("result" in o || "error" in o)) seen.add(o.id);
      } catch { /* not JSON-RPC; already forwarded */ }
    }
  }
  return seen;
}

async function handshakeAgain() {
  if (!initPayload) throw new Error("session lost before the client initialized");
  sessionId = null;
  const init = JSON.parse(initPayload);
  const res = await post(JSON.stringify({ ...init, id: "bridge-reinit" }));
  await res.text();
  const note = await post(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized", params: {} }));
  await note.text();
}

async function postOne(line, msg) {
  const id = msg.id;
  const isRequest = id !== undefined && id !== null && typeof msg.method === "string";
  try {
    let res;
    try {
      res = await post(line);
    } catch (e) {
      if (e.status !== 404 || msg.method === "initialize") throw e;
      logErr("session lost (404); re-initializing");
      reinit ??= handshakeAgain().finally(() => { reinit = null; });
      await reinit;
      res = await post(line);
    }
    const seen = relay(await res.text());
    if (isRequest && !seen.has(id)) {
      throw new Error("server closed the stream without answering");
    }
  } catch (e) {
    logErr(`${msg.method ?? "message"}:`, e.message);
    if (isRequest) replyError(id, e.message);
  }
}

try {
  await refreshAccessToken();
} catch (e) {
  logErr("startup:", e.message);
  process.exit(1);
}

let handshake = Promise.resolve(); // tail of the serialized handshake messages
const rl = readline.createInterface({ input: stdin, terminal: false });
rl.on("line", (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch (e) {
    logErr("dropping non-JSON stdin line:", e.message);
    return;
  }
  inflight++;
  let run;
  if (msg.method === "initialize" || msg.method === "notifications/initialized") {
    if (msg.method === "initialize") initPayload = line;
    run = handshake = handshake.then(() => postOne(line, msg));
  } else {
    const wait = handshake;
    run = wait.then(() => postOne(line, msg));
  }
  run.finally(() => { inflight--; maybeExit(); });
});
rl.on("close", () => { stdinClosed = true; maybeExit(); });
