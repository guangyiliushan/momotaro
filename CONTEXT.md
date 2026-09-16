# Momotaro

Local-first research workbench: humans author, the agent retrieves and proposes. This glossary is the domain language; it is not a spec.

## Sources and identity

**Workspace**:
A directory that contains `momotaro.toml` and owns exactly one database and the paper blobs beside it. It is the unit of CLI init and of 0.x data layout.
_Avoid_: vault, project, profile, app-data store (that is a 1.0 packaging concern)

**Vault**:
The single notes tree a Workspace points at. Indexing may only cover that root or a path inside it. Not the database, not a Project.
_Avoid_: library, knowledge base, corpus, second tree in the same workspace

**Source**:
One note, paper, or (later) web document the library can cite. Identity is a Source key, not a row id and not a live filesystem path. Renaming a note creates a new Source; old keys are not rewritten.

**Source key**:
A scheme-prefixed logical identity. 0.x schemes are `note:` and `arxiv:` only; DOI is metadata, not a key. The scheme-specific tail is NFC-normalized and never case-folded. Absolute paths and display names live elsewhere.
_Avoid_: canonical file path, absolute path, database id, URL-as-key, doi-as-key in 0.x

**Source revision**:
An immutable byte snapshot of a Source (`revision_hash` of normalized bytes). Updates append a revision; they do not overwrite. An arXiv vN is a new revision of `arxiv:<id>`, not a new Source. A journal DOI is related metadata, not a key. Missing bytes leave the row in place; opening fails as source integrity, never as a deleted revision.
_Avoid_: version (when you mean revision_hash), file, document, doi-as-source

**Chunk**:
A deterministic projection of one Source revision: the retrieval and citation locator. Once written for a `(source_key, revision_hash)`, rows are retained while any citation or context still points at them. Changing the hash function is a new index policy; old rows stay. A full wipe is disaster recovery under the current policy, not routine rebuild.
_Avoid_: canonical fact, Tantivy document, embedding, passage (when you mean the stored row)

**Origin class**:
Who authored the bytes: `owner` | `paper` | `web` | `agent` | `system`. Orthogonal to Source kind (`note` | `paper` | `web`).
_Avoid_: trust score, provenance (provenance is the citation five-tuple)

## Human desk

**Project**:
A named collection of Sources (and later annotations / pinned sparks). Deleting a Project does not delete Sources. It is a desk drawer, not a workspace and not a multi-tenant tenant.
_Avoid_: workspace, vault, folder, session

**Session**:
A conversation thread. It binds to zero or one Project at creation; that binding is immutable and sets default retrieval scope. It is not a Project and not a Run.
_Avoid_: chat, thread (when you mean Session), project

**Annotation**:
A human note pinned to `source_key + revision_hash + locator`. Origin class is `owner`. New revisions do not rebind it. It is not a retrieval hit; opening it locates bytes in that revision, not a chunk.
_Avoid_: comment, highlight, spark, memory, indexed document

**Spark**:
An agent-proposed, citation-backed card (`proposed` | `pinned` | `dismissed`). Pin is a human act and does not require citations. Only a pinned spark with at least one citation five-tuple is a retrieval source. 0.x actions are pin and dismiss only.
_Avoid_: memory, wiki page, note, idea (when you mean the stored card)

**Profile**:
A named bundle of Run defaults (model slots, policy, toolset). It is not memory, not notes, and not a 0.x record — 0.x has only `momotaro.toml`.
_Avoid_: persona, memory, user profile, `--profile` (that flag is timings)

## Work and answers

**Run**:
One accepted, auditable unit of work. Accepting a Run is not calling a model. Terminal status is driven by terminal events in the same transaction. 0.x modes are `ask`, `search` (explicit submit), `paper_add`, and `spark`. Index, feed poll, annotate, and typeahead are not Runs.
_Avoid_: request, job, agent loop, turn (a turn is one provider call inside a Run), ingest job

**Ask**:
A Run that retrieves in code, builds context in code, then makes at most one provider turn. The model has no tools in 0.x.
_Avoid_: agent, tool loop, chat completion (the product word is Ask)

**Same-library retrieval**:
One lexical index over owner notes and papers; one query returns mixed origin classes, ranked with origin boosts. v0.3 does not attach related notes via graph hops.
_Avoid_: 顺带返回, knowledge graph, hybrid search, 互检 as a second query

**Context scope**:
P1 is the bound Project's sources plus explicitly selected sources. With no Project, P1 is the whole Vault and the P1/P2 split is a no-op. P2 is the rest of the Vault. The split affects what enters model context, not what retrieval recorded.
_Avoid_: search filter, permission, tenant
