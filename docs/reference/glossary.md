# Glossary

**At-least-once delivery.** A message stays in its stream until a worker
acknowledges it; a worker that dies or takes longer than the acknowledgement
wait gets the message delivered again. Work is never lost, and may run twice.

**Bad source.** A conversion failure caused by the document itself (an HTML
page saved as `.pdf`, a broken or encrypted PDF, a paywall page). No backend
can convert it, so it is not retried by default. See
[conversion failure reasons](status-dashboard.md#conversion-failure-reasons).

**Catalog.** One YAML entry per document under `catalog/`, holding its
metadata, its conversion and embedding stamps, and its page offsets.

**Catalog stem.** The name that identifies a document everywhere: in
storage, in the catalog and in Qdrant. For a paper with a DOI it is the
lowercased DOI with `/` replaced by `_` (`10.1038_s41586-024-07487-w`).

**Chunk.** A passage of a converted document, about 1000 tokens, embedded as
one vector.

**CUDA.** NVIDIA's framework for computing on GPUs. The distill server
requires it.

**Durable consumer.** A named JetStream consumer the broker keeps between
connections. Every worker of one kind uses the same durable consumer
(`scribe-workers`, `distill-workers`), so they split the messages, and the
broker remembers which ones are still unacknowledged.

**Embedding.** A list of numbers (1024 for home-still's model) that captures
what a passage means. Passages that mean similar things have nearby
embeddings.

**Gateway.** `hs-gateway`, the one program reachable from the internet. It
checks each request's token and forwards it to the service on your network.

**Garage.** A self-hosted, S3-compatible object store that runs on small
hardware. One of the two storage backends.

**JetStream.** The persistence layer of NATS: streams that keep messages
until consumers acknowledge them.

**MCP.** The Model Context Protocol, a standard for letting AI assistants use
tools. `hs-mcp` exposes home-still as MCP tools.

**NATS.** The message broker that carries home-still's events between
machines.

**OAuth 2.1.** The authorisation standard Claude Desktop uses to get access to
your gateway without seeing a password.

**OCR.** Optical character recognition: reading the text in an image of text.

**Qdrant.** The vector database that stores the embeddings and finds the ones
nearest a query.

**Retryable failure.** A conversion failure caused by a backend (the model
looped, its stream died, it left gaps). A different or later backend may
succeed, so `hs pipeline reconvert-failed` re-queues these.

**S3.** Amazon's object-storage protocol; Garage speaks it.

**Scope.** What a gateway token may reach: `scribe`, `distill` or `mcp`. An
enrollment code fixes the scopes of every token it produces.

**Signed link.** A URL to one document (source or markdown), carrying an
expiry, a subject and an HMAC signature in its query string. The docs service
serves it without any other credential.

**Tier.** The `scribe.servers` entries that share one `backend` name, in the
order the backend first appears. A document goes to the first tier, to its
least-loaded server; if that tier fails it for a reason a different model
could fix, the next tier gets it.

**VLM.** A vision-language model: a model that reads an image and writes text.
The scribe server sends page regions to one (GLM-OCR by default).
