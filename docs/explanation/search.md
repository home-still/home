# How search works

A search question is rarely phrased the way the paper that answers it is
written. home-still's search is built so that a question finds the passage
whether it shares the passage's *meaning*, its *rare words*, or both.

## From markdown to points

The distill server prepares each converted document once, when it is
indexed.

1. **Chunking.** The markdown is cut into passages of about 1000 tokens, with
   100 tokens of overlap so a sentence that straddles a boundary is whole in
   one of them. Cuts fall on sentence boundaries and never cross a page
   separator, so every passage belongs to one page of the source.
2. **Embedding.** Each passage is turned into a 1024-dimensional vector by
   BGE-M3, a multilingual embedding model, running on the GPU. The model reads
   up to 1280 tokens, enough for the whole passage plus a short header, so no
   passage loses its tail.
3. **Storing.** Each passage becomes one *point* in Qdrant: its vector, its
   text, and the document's metadata (title, authors, year, DOI, page, line
   numbers). Point ids are derived from the document and the passage's
   position, so indexing a document again replaces its points instead of
   adding copies, and passages past a shortened document's new end are
   deleted.

   The metadata comes from the document's catalog entry, never from guessing
   at the text: the first DOI or year in a paper is usually a citation. A
   document indexed without a catalog entry carries no title until
   it is indexed again with one.

## Two channels, one ranking

Each search runs two retrievals over the same collection.

- **Dense.** The query is embedded with the same model and compared with
  every passage by cosine similarity. This finds passages that *mean* what
  the query means, even when they use none of its words.
- **Lexical.** A text index over the passages finds those that *contain* the
  query's words.

Neither is enough alone. A dense model represents "Hashimoto thyroiditis"
much like any thyroid passage and can lose the specific disease; a keyword
match cannot answer "how does attention scale with sequence length". The two
result lists are merged with **reciprocal rank fusion**: each passage scores
by its *rank* in each list, not by its raw score, because a cosine and a term
count are not on the same scale. The fused score orders hits within one
search; it means nothing when compared across searches.

**Which words the lexical channel requires** depends on how rare they are in
your library. The text match is an AND, and a real question is not: no
single passage contains every word of "Hashimoto thyroiditis euthyroid
patients", though the passage you want contains the rare ones. So the
lexical channel runs several small matches and pools their hits, most
specific first: the two rarest terms of the query; the query's first term
(usually what the question is about) with the rarest other term; the rarest
term alone; and the first term alone.

**Reference lists are kept apart.** To a dense model, a bibliography entry
about hippocampal neurogenesis *is* about hippocampal neurogenesis, so a
chunk of citations can outrank the passage that discusses the topic. Chunks
that are recognisably reference lists (lines with `doi:`, author names
followed by a year, `et al.`, page ranges, volumes, numbered entries) are
filed in a store of their own, `references`, when a paper is indexed, so a
search of the `academic` store returns only the passages that discuss the
topic. They are not thrown away: `hs distill search --store references`
searches them. A chunk that mixes citations with prose counts as prose.

## Searching whole papers: the abstracts store

Passage search answers "where is this said". To ask "which papers are about
this", a second store, `abstracts`, holds one record per paper: its title
and abstract. The abstract comes from the local
OpenAlex catalog when it has the paper, else from the provider's record in
the catalog, else from the markdown's `## Abstract` section; a paper with
none is skipped. `hs distill abstracts build` fills it, and an agent searches
it with `recall` on the `myelin` server.

## From a hit to the page

A hit names its document, page and the lines of its passage. When document
links are set up, each hit also carries signed links to the source and to
the markdown, for use on your network and away from it. The source link ends
in `#page=N`, which a PDF viewer opens at that page. The markdown link ends in
a text fragment, `#:~:text=…`, quoting the start of the passage, which a
browser scrolls to and highlights. A link expires, opens one document in one
format, and stops working if its subject is revoked; see
[open documents with links](../how-to/open-documents-with-links.md).
