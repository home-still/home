#!/usr/bin/env python3
"""Evaluate home-still retrieval against docs/loop/queries.yaml.

Reports, per query: the rank of the first hit whose text contains the
expected term (and, when `expect_doc` is set, whose doc_id matches), plus
whether the expected passage was found at all within `limit`.

Usage:
    python3 docs/loop/eval_queries.py [--limit 10] [--out FILE]

Talks to the distill server directly (HTTP) so it does not depend on the
CLI's presentation layer.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
QUERIES = os.path.join(REPO, "docs", "loop", "queries.yaml")
DEFAULT_SERVER = "http://127.0.0.1:7434"


def load_queries(path: str) -> list[dict]:
    """Minimal YAML reader for this file's fixed shape.

    PyYAML is not guaranteed to be installed; the file is simple enough
    that a small parser keeps this harness dependency-free.
    """
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
            if value == "null":
                value = None
            current[key] = value
    if current:
        queries.append(current)
    return queries


def distill_token() -> str | None:
    """The running distill server's backend token, read from its process env."""
    try:
        pids = subprocess.run(
            ["pgrep", "-f", "hs-distill-server"], capture_output=True, text=True
        ).stdout.split()
    except OSError:
        return None
    for pid in pids[:3]:
        try:
            with open(f"/proc/{pid}/environ", "rb") as fh:
                for entry in fh.read().split(b"\0"):
                    if entry.startswith(b"HS_BACKEND_TOKEN="):
                        return entry.split(b"=", 1)[1].decode()
        except OSError:
            continue
    return None


def search(server: str, token: str | None, query: str, limit: int) -> tuple[list[dict], float]:
    body = json.dumps({"query": query, "limit": limit}).encode()
    req = urllib.request.Request(
        f"{server}/search", data=body, headers={"Content-Type": "application/json"}
    )
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    start = time.time()
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            payload = json.loads(resp.read())
    except urllib.error.HTTPError as exc:
        raise RuntimeError(f"search failed: HTTP {exc.code}") from exc
    elapsed = (time.time() - start) * 1000
    hits = payload if isinstance(payload, list) else payload.get("hits", [])
    return hits, elapsed


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit", type=int, default=10)
    ap.add_argument("--server", default=DEFAULT_SERVER)
    ap.add_argument("--out")
    ap.add_argument("--queries", default=QUERIES)
    args = ap.parse_args()

    queries = load_queries(args.queries)
    if not queries:
        print("no queries parsed", file=sys.stderr)
        return 2

    token = distill_token()
    if not token:
        print("warning: no HS_BACKEND_TOKEN found; search may be refused", file=sys.stderr)

    rows = []
    for q in queries:
        expect = (q.get("expect") or "").lower()
        expect_doc = q.get("expect_doc")
        try:
            hits, ms = search(args.server, token, q["query"], args.limit)
        except Exception as exc:  # noqa: BLE001 - reported per query
            print(f"  {q['id']}: ERROR {exc}", file=sys.stderr)
            rows.append({"id": q["id"], "group": q.get("group"), "error": str(exc)})
            continue

        found_rank = None
        doc_rank = None
        for i, hit in enumerate(hits):
            text = (hit.get("chunk_text") or "").lower()
            if found_rank is None and expect and expect in text:
                found_rank = i + 1
            if (
                doc_rank is None
                and expect_doc
                and expect_doc.lower() in (hit.get("doc_id") or "").lower()
            ):
                doc_rank = i + 1
        # A doc-anchored query passes only if the right doc is in the results.
        if expect_doc:
            passed = doc_rank is not None
        else:
            passed = found_rank is not None
        rows.append(
            {
                "id": q["id"],
                "group": q.get("group"),
                "query": q["query"],
                "expect": q.get("expect"),
                "expect_doc": expect_doc,
                "rank": found_rank,
                "doc_rank": doc_rank,
                "passed": passed,
                "ms": round(ms, 1),
                "top_score": (hits[0].get("score") if hits else None),
            }
        )
        flag = "PASS" if passed else "MISS"
        rank = doc_rank if expect_doc else found_rank
        print(
            f"  {q['id']} [{q.get('group')}] {flag:4} rank={rank if rank else '-':>4} "
            f"ms={ms:7.1f}  {q['query'][:52]}"
        )

    total = len(rows)
    passed = sum(1 for r in rows if r.get("passed"))
    print()
    print(f"RECALL@{args.limit}: {passed}/{total} = {passed / total * 100:.1f}%")
    for group in sorted({r.get("group") for r in rows if r.get("group")}):
        gr = [r for r in rows if r.get("group") == group]
        gp = sum(1 for r in gr if r.get("passed"))
        print(f"  {group:8} {gp}/{len(gr)} = {gp / len(gr) * 100:.1f}%")
    times = [r["ms"] for r in rows if "ms" in r]
    if times:
        print(f"  median latency {sorted(times)[len(times) // 2]:.0f} ms")
    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump({"rows": rows, "recall": passed / total}, fh, indent=2)
        print(f"  wrote {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
