# home-still: a plain-English guide

> **A personal research engine that turns the world's open academic literature into a library you can actually use — find papers, read them, ask questions, get grounded answers.** Runs on your own hardware. Costs nothing in subscriptions.

---

## 1. Why home-still exists

Most of humanity's high-quality knowledge — the stuff that has been peer-reviewed, replicated, and argued over for decades — sits in academic papers. Tens of millions of those papers are openly licensed and free to read. And almost no one outside of a university or a well-funded company can actually *use* them.

The barriers are quiet but enormous:

- **Search is keyword-based.** Web search engines and library indexes match the words you type, not the *idea* you mean. If you don't already know the jargon for what you're looking for, you can't find it.
- **PDFs are unreadable to software.** A paper is a picture of text. Pulling clean text out of a column-formatted PDF with figures and tables is hard enough that most tools just give up.
- **Reading takes hours.** Even when you find a relevant paper, you have to read it carefully to know whether it answers your question.
- **Commercial tools cost money you may not have.** The good ones charge per seat per month and lock the corpus behind their account.

home-still flips all four. It's a small set of programs that, together, give *one person* — sitting at home, on cheap hardware they probably already own — their own personal academic search engine. It gathers papers from open sources, reads them with a vision model, organizes them so they're searchable by *meaning* (not just words), and exposes the whole library to an AI assistant so you can ask questions in plain English and get answers grounded in real papers.

That's what we mean by **democratizing the conversion of high-quality knowledge into actionable information.** Not gating it behind institutions, subscriptions, or technical degrees. Putting it on a Raspberry Pi in your closet.

## 2. Who this is for

- **Independent researchers and writers** who need to understand a literature, not just locate it.
- **Students** who want a personal library that doesn't disappear when they graduate.
- **Hobbyists and citizen scientists** who go deep on niche topics and need real papers, not blog posts.
- **Journalists and policy analysts** who need to ground claims in primary sources.
- **Anyone curious** who finds themselves reading abstracts on arXiv at midnight and wants a better tool.

It is explicitly **not** built for institutions with the budget for commercial alternatives. It's built for people who'd otherwise have nothing.

## 3. What it actually does — the four stages

home-still works in four stages. You can use them all together, or just the ones you need.

```
   ┌─────────────┐    ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
   │  1 Acquire  │──▶ │  2 Convert  │──▶ │  3 Index    │──▶ │  4 Search   │
   │  hs paper   │    │  hs scribe  │    │  hs distill │    │  hs distill │
   │             │    │             │    │             │    │   search    │
   │  6 open     │    │  PDF → clean│    │  text → a   │    │  ask in     │
   │  databases  │    │  markdown   │    │  vector DB  │    │  plain      │
   │             │    │  via VLM    │    │             │    │  English    │
   └─────────────┘    └─────────────┘    └─────────────┘    └─────────────┘
        │                  │                  │                  │
        ▼                  ▼                  ▼                  ▼
   PDFs land in      Markdown lands     Vectors land in      Snippets,
   ~/home-still/     in markdown/       Qdrant, ranked      ranked by
   papers/           with figures,       by meaning           meaning, with
                     tables, formulas                         citation back
                     preserved                                to the paper
```

Each stage is a separate command, and each one runs *automatically* once the next thing arrives. Drop a PDF into the watched folder and a few minutes later it's converted, indexed, and searchable. You don't manage the pipeline; it manages itself.

### Stage 1 — Acquire (`hs paper`)

This is the gathering step. `hs paper search` queries **six free academic databases at once**:

- **arXiv** — physics, math, computer science, biology preprints
- **OpenAlex** — 250 million works across every field
- **Semantic Scholar** — 200 million papers, including citation graphs
- **Europe PMC** — biomedical and life sciences
- **CrossRef** — 147 million DOI records
- **CORE** — 300 million open-access papers

The results come back deduplicated and ranked. You can then download the PDFs of the ones that look interesting — `hs paper download "transformer attention" -n 25` grabs the top 25 papers about transformer attention.

This is the moment a paper becomes *yours*: a file on your disk, in your control, that won't disappear if a publisher decides to paywall it tomorrow.

### Stage 2 — Convert (`hs scribe`)

A PDF is a picture of text. To search a paper by meaning, you first need to extract the text — including figures, tables, equations, and the right reading order across multi-column layouts.

`hs scribe` does this with a **two-stage pipeline**:

1. A **layout detector** looks at each page and identifies regions: title, paragraphs, figures, tables, equations, footnotes. It gets the reading order right even on tricky two-column papers.
2. A **vision-language model (VLM)** reads each region. Tables are reconstructed cell-by-cell. Equations come out in proper format. The result is clean markdown that a human or a program can read.

Behind the scenes, this needs a GPU to be fast — Apple Silicon (Metal), an NVIDIA card (CUDA), or a strong CPU as a last resort. `hs scribe init` figures out what you have and configures itself.

This is the moment a paper becomes *readable* — not just visible, but parseable, searchable, and quotable.

### Stage 3 — Index (`hs distill`)

Markdown is just text. To search by meaning rather than by exact words, the text needs to be turned into **vectors** — long lists of numbers that capture what each chunk of the paper is *about*.

`hs distill` does three things:

1. **Chunks** the markdown into bite-sized pieces (about 1,000 tokens each, with some overlap so context isn't lost at the seams).
2. **Embeds** each chunk using a model called BGE-M3, which produces a 1,024-dimensional vector. Chunks about the same topic end up close together in vector space, no matter what words they used.
3. **Stores** the vectors in **Qdrant**, a database designed to find "vectors near this one" extremely quickly.

This is the moment a paper becomes *findable by idea*, not just by keyword. A paper that uses the phrase "self-attention mechanism" and a paper that uses "scaled dot-product over query-key projections" can be retrieved by the same query, because they mean the same thing.

### Stage 4 — Search (`hs distill search`)

This is the payoff. You type a query in plain English, your query gets embedded into the same vector space, and the database finds the chunks closest to it in *meaning*. Results come back with title, authors, year, a snippet of the relevant text, and a relevance score.

```sh
hs distill search "what makes attention computationally expensive"
```

You don't have to use the right buzzwords. You don't even have to know the field. If a paper in your library says something close to what you asked, it will surface.

This is the moment knowledge becomes *answerable*.

## 4. Your first session, end to end

Install:

```sh
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
```

That puts a single binary called `hs` in `~/.local/bin/`. Make sure that directory is on your `PATH`.

Set up your config:

```sh
hs config init
```

This walks you through a one-time setup and writes `~/.home-still/config.yaml`. You can hand-edit it later.

Search for something you actually care about:

```sh
hs paper search "diffusion models for protein structure" -n 10
```

You'll see ten papers, most relevant first, with title, authors, year, citation count, and DOI. Pick a query that suits you.

Download what looks good:

```sh
hs paper download "diffusion models for protein structure" -n 10
```

The PDFs land in `~/home-still/papers/`. If you have `hs serve scribe` and `hs serve distill` running (locally or on another machine in your network), the conversion and indexing kick off automatically — no extra commands.

Watch the live dashboard:

```sh
hs status
```

You'll see PDF count climb, then markdown count climb, then embedded chunk count climb. When the chunks-embedded number stops moving, your library is ready to search.

Ask a question:

```sh
hs distill search "what advantage do diffusion models have over autoregressive models for proteins"
```

You get back the most relevant snippets across the papers you just downloaded, ranked by meaning, with a pointer back to each source paper. That's the loop.

## 5. Talking to your library through Claude

Search returns snippets. Sometimes you don't want snippets — you want a *synthesis*. You want to ask a real question and have something read across multiple papers and answer it for you, with citations.

That's what the **MCP server** is for.

**MCP** stands for *Model Context Protocol*. It's a way for AI assistants like Claude to use external tools. home-still ships an MCP server (`hs-mcp`) that exposes thirteen tools to Claude:

- `paper_search` — search the six databases
- `paper_get` — look up by DOI
- `catalog_list` / `catalog_read` — browse what's in your library
- `markdown_list` / `markdown_read` — read the actual papers
- `scribe_convert` — convert a PDF
- `distill_search` — semantic search across your library
- `distill_status` / `distill_exists` — check what's indexed
- `system_status` / `scribe_health` — pipeline health

Once you wire it up, Claude can search your library, read papers, and answer questions grounded in *your specific corpus*. You can ask "summarize the disagreement between these three papers on X" and Claude will actually go read them and give you an answer with citations back to the source.

There are two ways to wire it up:

- **Local** — you run Claude Desktop on the same machine as home-still. One line in Claude's config and you're done.
- **Remote** — your library lives on a home server, and you want to reach it from your laptop, your phone, anywhere. This uses a Cloudflare tunnel and OAuth2 (the same authorization standard that signs you into your bank). Claude opens a browser, you paste in a one-time enrollment code, and from then on Claude has access. See the [deployment guide](deployment.md) for the full setup.

This is the moment knowledge becomes *actionable*. Not "here are some snippets" — but "here's a synthesized answer to your question, drawn from papers you can verify."

## 6. The shape of a real-world setup

Most people start by running everything on one computer. That works fine for a few hundred papers.

If your library grows — or if you want to keep it on a small home server so you can reach it from anywhere — home-still is designed to spread across machines. A typical small setup looks like this:

- A **storage server** (a Raspberry Pi with a USB SSD attached) holds the papers and the converted markdown. Other machines mount it like a network drive.
- A **GPU machine** (an old gaming PC with an NVIDIA card) runs the conversion and embedding work. This is the only machine that needs serious horsepower.
- A **database host** runs Qdrant (the vector database) and optionally Postgres (for paper metadata).
- A **tunnel host** runs Cloudflare's tunnel agent and the home-still gateway. This is what makes your library reachable from outside your house, securely, without opening any ports on your router.
- **Client machines** (your laptop, your desktop) run the `hs` CLI and search/read the library.

The total hardware cost for a working cluster is in the low hundreds of dollars if you start from scratch, or zero if you already have a Pi and an old GPU box. The full step-by-step is in the [home-cloud deployment guide](deployment.md).

## 7. Glossary

**Vector embedding** — Turning a piece of text into a list of numbers (typically 1,024 of them) that captures what the text *means*, not just what words it uses. Two embeddings are "close" if their texts mean similar things.

**Qdrant** — A database specifically designed to find embeddings that are close to a given embedding, fast, even with hundreds of millions of them.

**OCR** — Optical Character Recognition. Pulling text out of a picture of text. Old OCR was rule-based and brittle. Modern OCR uses deep learning and works much better, especially on academic PDFs with weird layouts.

**VLM** — Vision-Language Model. A neural network that can look at an image and produce text describing it, transcribing it, or reasoning about it. home-still uses one called GLM-OCR to read each region of a PDF page.

**MCP** — Model Context Protocol. An open standard for letting AI assistants like Claude use external tools. home-still exposes its API as MCP tools so Claude can search your library and read your papers.

**Gateway** — A small program that sits between the public internet and your home network. It checks that requests are authorized (via OAuth2 or a bearer token) and forwards the legitimate ones to the right service on your LAN. Means you don't have to open ports on your router.

**OAuth2** — The authorization standard your bank, your email, and now Claude all use. Lets a third party (like Claude Desktop) get permission to use a service (your home-still library) without ever seeing your password.

**NFS** — Network File System. A protocol for mounting a remote drive as if it were local. home-still uses NFS to let multiple machines share the papers folder.

**S3 / Garage** — S3 is Amazon's object-storage protocol. **Garage** is a free, self-hosted implementation of that same protocol that runs on cheap hardware (including a Raspberry Pi). home-still can store papers either on a regular filesystem (NFS) or on Garage S3, depending on what fits your setup.

**CUDA** — NVIDIA's framework for running computation on GPUs. The conversion (scribe) and embedding (distill) stages are dramatically faster on a CUDA GPU than on a CPU. home-still requires it for the embedding stage and strongly prefers it for conversion.

## 8. Where to go next

Once you've used home-still for an hour and want to go deeper:

- **Run it across multiple machines** — see [`deployment.md`](deployment.md) for a step-by-step home-cloud setup.
- **Customize the paper search** — see the [paper search section in the root README](../README.md#paper-search).
- **Tune the PDF conversion pipeline** — see [`crates/hs-scribe/README.md`](../crates/hs-scribe/README.md).
- **Tune chunking, embedding, and search** — see [`crates/hs-distill/README.md`](../crates/hs-distill/README.md).
- **Set up secure remote access for Claude Desktop** — see [`crates/hs-gateway/README.md`](../crates/hs-gateway/README.md).
- **Wire it into your AI assistant** — see [`crates/hs-mcp/README.md`](../crates/hs-mcp/README.md).

---

The point of home-still isn't the technology. The technology is the means. The point is that one person, at a kitchen table, can build their own research engine over millions of open-access papers — and use it to ask real questions and get grounded answers. That's the gap between *knowledge exists somewhere* and *you can actually act on it*. Closing that gap is the entire mission.
