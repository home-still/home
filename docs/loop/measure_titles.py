#!/usr/bin/env python3
"""Measure the share of search hits that carry a title.

Runs the fixed query set (`docs/loop/queries.yaml`) against the distill
server and reports, over every hit returned, how many carry a title.

This is the measure the title backfill is judged by: an agent that calls
`distill_search` can only name the document it found a passage in when
the hit carries a title.

Usage:
    python3 docs/loop/measure_titles.py [--limit 10] [--out FILE]
"""

from __future__ import annotations

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
QUERIES = os.path.join(REPO, "docs", "loop", "queries.yaml")
DEFAULT_SERVER = "http://127.0.0.1:7434"


def load_queries(path: str) -> list[dict]:
    """Minimal YAML reader for this file's fixed shape (see eval_queries.py)."""
    queries: list[dict] = []
    current: dict | None = None
    with open(path, encoding="utf-8") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            stripped = line.strip()
            if stripped.startswith("- "):
                if current:
                    queries.append(current)
                current = {}
                stripped = stripped[2:].strip()
                if not stripped:
                    continue
            if current is None:
                continue
            m = re.match(r'^(\w+):\s*(.*)$', stripped)
            if not m:
                continue
            key, value = m.group(1), m.group(2).strip()
            if value.startswith('"') and value.endswith('"'):
                value = value[1:-1]
            if value.startswith("'") and value.endswith("'"):
                value = value[1:-1]
            current[key] = value
    if current:
        queries.append(current)
    return queries


def backend_token() -> str | None:
    """The shared backend secret, from the running server's environment."""
    for name in ("hs-distill-server", "hs"):
        try:
            out = subprocess.run(
                ["pgrep", "-f", name], capture_output=True, text=True
            ).stdout.split()
        except OSError:
            continue
        for pid in out:
            try:
                with open(f"/proc/{pid}/environ", "rb") as fh:
                    for entry in fh.read().split(b"\0"):
                        if entry.startswith(b"HS_BACKEND_TOKEN="):
                            return entry.split(b"=", 1)[1].decode()
            except OSError:
                continue
    return None


def search(server: str, query: str, limit: int, token: str | None) -> list[dict]:
    body = json.dumps({"query": query, "limit": limit}).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(f"{server}/search", data=body, headers=headers)
    with urllib.request.urlopen(req, timeout=60) as resp:
        payload = json.load(resp)
    # The server answers with the hit list itself; older shapes wrapped it.
    if isinstance(payload, list):
        return payload
    return payload.get("results", [])


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit", type=int, default=10)
    ap.add_argument("--server", default=DEFAULT_SERVER)
    ap.add_argument("--out")
    args = ap.parse_args()

    queries = load_queries(QUERIES)
    token = backend_token()
    total = 0
    titled = 0
    per_query = []
    latencies = []

    for q in queries:
        query = q.get("query", "")
        if not query:
            continue
        start = time.time()
        try:
            hits = search(args.server, query, args.limit, token)
        except urllib.error.URLError as e:
            print(f"  {q.get('id', '?')}: search failed: {e}", file=sys.stderr)
            continue
        latencies.append((time.time() - start) * 1000)
        hit_titled = 0
        for h in hits:
            total += 1
            title = h.get("title")
            if isinstance(title, str) and title.strip():
                titled += 1
                hit_titled += 1
        per_query.append(
            {
                "id": q.get("id"),
                "group": q.get("group"),
                "hits": len(hits),
                "titled": hit_titled,
            }
        )

    share = (titled / total * 100) if total else 0.0
    missing = total - titled
    print(f"hits: {total}")
    print(f"hits WITH a title:    {titled} = {share:.1f}%")
    print(f"hits WITHOUT a title: {missing} = {100 - share:.1f}%")
    if latencies:
        print(f"median latency: {statistics.median(latencies):.0f} ms")

    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump(
                {
                    "hits": total,
                    "titled": titled,
                    "share": share,
                    "per_query": per_query,
                    "median_latency_ms": statistics.median(latencies)
                    if latencies
                    else None,
                },
                fh,
                indent=2,
            )
        print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
