---
title: What Momotaro is
description: A local-first research workbench where people judge, write and annotate while the agent retrieves, compares and proposes — never the other way around.
type: concept
sidebar_position: 1
---

# What Momotaro is {/* #what-momotaro-is */}

Momotaro is a local-first research workbench for serious study. It turns your
notes and papers into a desk that is searchable, citable and verifiable: you
judge, annotate and write; the agent retrieves, compares and proposes.

> Deterministic first. AI where it matters. Every answer traceable.

## What it is not {/* #not */}

- **Not a general research agent.** Momotaro does not run a task to the end on
  your behalf — no unattended experiments, no papers written for you.
- **Not a chat product.** There is no product web backend; the workbench is
  files on your machine and one local database.
- **Not a knowledge graph.** Retrieval is lexical and explainable; the model has
  no tools and no free loop.
- **Not a memory system.** Nothing the agent produces becomes a fact by itself.
- **Not a team tool, yet.** 0.x is one person, one machine: workspaces are
  single-user, and collaboration is an explicit future topic — not a hidden
  default.

## The desk, not the driver {/* #desk-not-driver */}

| | A general research agent | Momotaro |
|---|---|---|
| Lead | the agent runs the process | the person judges; the agent assists |
| Success | the task closes, a report appears | evidence is traceable, annotations accumulate, sparks land in a project |
| Default action | search, download, write on its own | show first; writing, downloading and importing need your nod |
| Knowledge | the model's word | only your writing is fact; AI output stays proposed |
| Unit of work | a free tool loop | a project is the desk; a run is one auditable piece of work |

## Three commitments {/* #commitments */}

1. **Deterministic first.** Searching, chunking and citation checks consume no
   model tokens. The model is used where judgment is genuinely needed — at
   most one turn per [ask](../glossary.md#ask).
2. **People author.** Your notes and annotations are the only facts. What the
   agent proposes stays *proposed* until a person pins it; a
   [spark](../glossary.md#spark) becomes a source only by human action.
3. **Every answer traceable.** An answer carries
   [citations](../glossary.md#citation), and each citation points at exact
   bytes that were verifiable at the time of the [run](../glossary.md#run).

## Where it is going {/* #direction */}

The 0.x line is the engine phase, driven through the command line. The first
full release is a cross-platform desktop app plus the CLI; a read-mostly
mobile companion follows later. There is no product web backend — nothing
leaves your machine except what you choose to publish.

## Next {/* #next */}

The concepts behind the desk — sources, revisions, chunks, runs — are
collected in [the domain model](domain-model.md).
