---
title: Glossary
description: One word per concept, defined once — the vocabulary every page draws from.
type: reference
sidebar_position: 6
---

# Glossary

One word per concept, defined once. A term links here at its first appearance on a page.

This page is generated from `terminology.yaml`. Edit that file, not this one.

## workspace

A directory that contains `momotaro.toml` and owns exactly one database, with the paper bytes beside it.

_Avoid_: vault, project, profile, app-data store.

## vault

The single notes tree a workspace points at.

_Avoid_: library, knowledge base, corpus.

## source

One note, paper, or web document the library can cite; its identity is the source key, not a row id and not a live filesystem path.

_Avoid_: file, document.

## source key

The scheme-prefixed identity of a source (`note:` / `paper:`), NFC-normalized and never case-folded. The scheme names a source type, never a provider.

_Avoid_: canonical file path, database id, URL as key, provider as scheme.

## source revision

An immutable byte snapshot of a source; updates append a new revision instead of overwriting.

_Avoid_: version, file, document.

## chunk

A deterministic projection of one source revision; the unit retrieval finds and citations locate.

_Avoid_: passage, embedding, canonical fact.

## span

A quoted stretch of source bytes an answer is built from. An answer is a sequence of spans; the words between them are a fixed connector that claims nothing.

_Avoid_: quote, citation, excerpt, snippet.

## origin class

Who authored the bytes: `owner` | `paper` | `web` | `agent` | `system`. It records authorship, not reliability; how checkable a claim is belongs to a separate, display-only axis.

_Avoid_: trust score, provenance, authority, reliability score.

## project

A named collection of sources (and, later, annotations and pinned sparks); deleting it never deletes sources.

_Avoid_: workspace, folder, tenant.

## session

A conversation thread bound to zero or one project at creation; that binding sets the default retrieval scope.

_Avoid_: chat, thread, project.

## annotation

A human note pinned to a source revision and locator; its origin class is always `owner`.

_Avoid_: comment, highlight, memory.

## spark

An agent-proposed, citation-backed card (`proposed` | `pinned` | `dismissed`); only a pinned spark with a citation is a retrieval source.

_Avoid_: memory, idea, wiki page.

## profile

A named bundle of run defaults (model slots, policy, toolset); not memory and not notes.

_Avoid_: persona, memory, user profile.

## run

One accepted, auditable unit of work; its terminal status is driven by terminal events recorded with it.

_Avoid_: job, request, agent loop.

## ask

A run that retrieves and builds context in code, then makes at most one provider turn.

_Avoid_: agent, tool loop, chat completion.

## same-library retrieval

One lexical index over owner notes and papers; a query returns mixed origin classes, ranked with origin boosts.

_Avoid_: hybrid search, knowledge graph.

## context scope

P1 (the bound project's sources, or the whole vault when unbound) versus P2 (the rest); it shapes what enters model context, not what retrieval recorded.

_Avoid_: search filter, permission, tenant.

## citation

The five-tuple that anchors a claim to bytes: source key, revision, locator, quote, hash.

_Avoid_: reference, footnote, link.

## verification

Checking a citation against the recorded bytes; a failure is shown as a first-class state, never suppressed.

_Avoid_: fact check, spell check.

## trace

The recorded reconstruction of a run — query, evidence, prompt, model answer and verdict — replayable after the fact.

_Avoid_: log, history.

## feed

An RSS / Atom / JSON Feed source polled by the workspace, imported via OPML; feed papers enter the library through a confirmation gate.

_Avoid_: reader, newsletter.
