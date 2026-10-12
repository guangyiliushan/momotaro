---
title: Writing guide
description: The rules Momotaro pages and code comments follow — documentation principles, page contract, voice, accessibility, terminology and translation.
type: meta
sidebar_position: 5
---

# Writing guide {/* #writing-guide */}

This site is written for people, not for the codebase. Code changes
constantly; the pages here describe the stable things — what the product is,
what its concepts mean, how its workflows look — and never cite files,
symbols, or line numbers. A fact may be checked against the repository while
a page is being written, but no reader ever needs the repository to
understand it.

## Documentation principles {/* #principles */}

These rules outrank style. They are what keeps the site correct while the
code keeps changing.

The project never does these things:

1. cite documentation from code — comments, messages and identifiers in
   source files never name documents, sections or records; comments explain
   the code itself;
2. put source-level locations into the site — no file paths, symbol names,
   line numbers or "see the source" links on any page;
3. point the site at files outside the repository — research notes, local
   archives and drafts never flow into published pages;
4. leave process traces — no task numbers, review rounds, probe IDs or
   acceptance vocabulary; pages state facts, not the process that produced
   them;
5. paste implementation code as the truth — examples show commands,
   configuration and outputs, never the internals of the implementation;
6. keep two sources for one fact — terms come from `terminology.yaml`, the
   command surface from the CLI reference, generated pages from their
   generators; when a copy and its source disagree, the source wins;
7. cut decisions out into standalone numbered records — architecture
   decision records and the like fragment knowledge, outlive their context
   and rot; a decision is told where it applies, with its reasons and
   trade-offs, in the flow of the material.

These are always allowed:

1. describing product behaviour in full — commands, configuration,
   workflows, meanings: that is what the site is for;
2. checking facts against the repository while writing — the check stays
   valid even though no path is ever printed;
3. keeping the site in the same repository, branch and review flow as the
   code;
4. letting the code's own API documentation serve its own readers — the
   site and the in-code documentation each stand alone;
5. naming components in plain words — ingest, storage, retrieval, context,
   verification — including in diagrams: what a component does, never
   where it lives.

One exception stays closed on purpose: a future contributor-facing section
could cite code the way developer guides in other projects do. That switch
flips only by an explicit decision — no page may drift into it.

## Languages {/* #languages */}

- English is the default locale; Simplified Chinese (`zh-CN`) and Japanese
  (`ja`) are peers: every locale lives under its own path segment, and no
  language sits at the site root.
- Drafting happens in Chinese. The English and Japanese pages are produced
  from the finished Chinese draft — a first pass by machine, then an editing
  pass for natural reading — and are treated as equals once they land.
- A page may ship in English first. Missing translations fall back to
  English until a milestone (such as a release) requires full parity.

## The page contract {/* #page-contract */}

Every page:

1. opens with one sentence that says what the page is for;
2. carries a `description` in its front matter — one sentence, at most 150
   characters; it is what search results and the machine-readable index show;
3. gives every heading an explicit ID (`## A title {/* #an-id */}`) so that
   links survive translation, which would otherwise change the auto-generated
   anchors;
4. covers exactly one idea; a page that answers two questions is two pages;
5. names its own prerequisites when it has any, before the first step;
6. declares its `type:` in the front matter — `concept`, `task`,
   `reference`, `troubleshooting` or `meta`.

## Voice and style {/* #voice */}

English pages follow the conversational, second-person, active-voice,
present-tense register of the Google developer documentation style guide:
address the reader as *you*, put the condition before the instruction
("To delete a project, …"), prefer short sentences, use sentence-case
headings, and never use "click here" — link text always describes its
target. Keep every page's opening sentence under 200 characters.

Chinese pages follow the standard Chinese technical-writing conventions:
a space between Chinese and Latin/numeric runs, full-width punctuation inside
Chinese sentences, correct casing for product names (`Momotaro`, `GitHub`,
`SQLite`), and no stacked punctuation. These rules are mechanically checked
(zhlint and autocorrect) once the lint step lands in CI.

Japanese pages follow the same principles; once the first Japanese content
lands, its pass gets the same treatment — machine first, editing pass second.

## Accessibility {/* #accessibility */}

- Every informative image carries alt text that describes what the image
  *does*, not what it looks like; decorative images use an empty `alt=""`.
- Alt text stays under 150 characters and does not begin with "image of".
- Structure is expressed with headings, never with bold text; heading levels
  are never skipped.
- No directional-only phrasing ("above", "on the right"): refer to things by
  name or position in a list.
- Write out `and` as a word, not `+`; screen readers handle words.

## Diagrams {/* #diagrams */}

- Diagrams are written as text (Mermaid) and live in the page like any
  other content; they translate with it.
- Every diagram carries a title (`accTitle`) and a description
  (`accDescr`) for readers who use assistive technology, and the sentence
  next to it says what it shows — a page must read completely with every
  diagram skipped.
- Keep a diagram under ten nodes. Labels come from this site's
  vocabulary — component names in plain words — never file names, symbol
  names or code shorthand.
- A diagram is added only when it earns its place: a structure, a flow or
  a lifecycle that the prose genuinely benefits from seeing drawn.

## Terminology {/* #terminology */}

- The single source of terms is `terminology.yaml`; the glossary page is
  generated from it. Never introduce a second word for a concept that is
  already named — pick one term and use it everywhere.
- A term's first appearance on a page links to the glossary.
- When a term in Chinese has more than one common rendering, `terminology.yaml`
  decides; the decision is written down, not re-argued per page.

## Translation-friendly by construction {/* #translation-friendly */}

Beyond explicit heading IDs: avoid idioms, cultural references, humour and
seasonal examples; write dates and units unambiguously; let one sentence
carry one instruction. These rules exist so that the Chinese draft, the
English page and the Japanese page can all be reviewed by the same
checklist.

## Code comments and names {/* #comments */}

Source files follow the same discipline as pages do.

### Names {/* #names */}

Identifiers — modules, types, functions, constants, variables, tests,
fixtures — never carry version codenames. A version number exists only as a
value: in wire spellings, constants, migration step numbers, file-name
templates. When a historical artifact genuinely must mention a schema
version in its name, it spells the version out — `…_at_schema_2`, never
`…_v2_…`. Process numbers (waves, review rounds) never enter code at all:
identifiers, comments and test messages carry the fact, not where it
happened.

### Comments {/* #comment-rules */}

1. Layer by audience: the contract a caller sees goes in `///` doc comments
   (`//!` at the head of a crate or module); implementation motives,
   trade-offs and invariants go in `//`, which never renders into API
   documentation.
2. Explain why, never translate the code; do not restate what the type
   system already says. The opening line of a doc comment is a one-sentence
   summary, at most 200 characters.
3. Document sections match the signature: a `Result` return carries
   `# Errors` (which case, which error), a function that can panic carries
   `# Panics`, an `unsafe fn` carries `# Safety` with every caller
   obligation as a list item, and anything with a meaningful usage pattern
   gives `# Examples` (examples lean on `?`; boilerplate hides behind `#`).
4. Every `unsafe {}` block is preceded by `// SAFETY:`, saying why this
   site satisfies each obligation the surrounding contract imposes.
5. Never: commented-out code; `/* … */` banners (use `////` when a break is
   truly needed); comments inside signature lines; `#` headings inside `//`
   comments; empty doc comments or dangling blank lines after them.
6. A comment never cites a document — no section symbols, no document
   names, no record numbers — and never carries task or review
   identifiers. Facts are stated where they live: documents get
   reorganised, references rot.
7. A version behaves in comments exactly as in names: a value, never a
   codename. Write `schema 3`, not `v3`.
8. `TODO` and `FIXME` exist only as `// TODO(#<issue>, <owner>): item`;
   anything without a tracking source is recorded elsewhere, never parked
   in code.
9. Links and width: doc comments reference code as `[Item]` and write
   external URLs as `<https://…>`; lines stay within 100 columns; crate
   roots enable the `rustdoc::all` and `rustdoc::unescaped_backticks`
   warnings.

## Review and tooling {/* #review */}

- Chinese pages are checked by zhlint and autocorrect; English pages by Vale
  (Google style package plus a project vocabulary) once English editing
  starts. The checks run in CI — staged in as non-blocking warnings first,
  promoted to blocking once their noise is tuned down.
- The `description` requirement is checked by the build itself; a missing or
  over-long description fails.
- A page is done when it passes the build, reads naturally in every language
  it ships in, and a reviewer has walked its steps at least once.
