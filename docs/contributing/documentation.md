# Documentation guide

Documentation has two audiences. End-user material explains installing, configuring, running, and extending Nefor. Contributor material explains changing, testing, and releasing this repository. Keep them separate.

## Authority and placement

- `README.md`, `starter/README.md`, component READMEs, and public behavior references under `docs/` are end-user-facing.
- `docs/contributing/` is maintainer-facing procedure and code ownership.
- `CLAUDE.md` is agent/contributor working context, not the public manual.
- Design records and investigations may remain in `docs/` when useful, but must carry a banner saying they are historical/internal and name the current authority.
- `CHANGELOG.md` is release history, not a substitute for current documentation.

Code and current tests are authoritative for behavior. `docs/architecture.md` is the concise architecture authority, and `docs/contributing/architecture.md` is its ownership map. MAG's current implementation references live in `crates/nefor-mag`, `plugins/mag/docs`, and the shipped MAG library/corpus; historical refactor documents are not contracts.

## Freshness rules

Update documentation in the same commit when changing any of these:

- user-visible commands, flags, environment variables, defaults, install layout, or provider behavior;
- public Lua, NCP, plugin, MAG, configuration, or extension contracts;
- code ownership, layer boundaries, persistence semantics, or compatibility policy;
- just recipes, confidence tiers, CI gates, release targets, bundle contents, tagging, or tap automation;
- examples, expected diagnostics, or known limitations that the change resolves.

When editing a document, verify every command against `just --list` or the owning CLI, every path against the tree, and every quantitative claim against reproducible evidence. Avoid volatile test counts and timings. Link to the canonical recipe instead of duplicating its Cargo command list. Remove stale guidance rather than stacking a correction below it.

## Review checklist

1. Identify the audience and current authority.
2. Search the repository for old names, paths, flags, version numbers, and contradictory descriptions affected by the change.
3. Keep current-state guidance in authoritative docs; put release facts in the changelog and rationale/history in a clearly labeled design record.
4. Run `just fmt` for Markdown formatting, inspect all resulting changes, and stage only owned files.
5. Check local relative links in changed Markdown and report known stale references outside the change's ownership.

Documentation-only changes normally need formatting plus structural/link inspection. Run product tests when the docs change executable examples, fixtures, scripts, or generated/reference content used by tests.
