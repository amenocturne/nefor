# MAG inside Nefor

Nefor is a live application of MAG. Its libraries define typed graph data,
construct higher-level nodes from lower-level actors, validate the result, and
return an immutable program or delta artifact.

This section describes that application in terms of the
[core language](<../01. core/README.md>). Graphs, nodes, agents, reasoners,
worktrees, and development cycles are Nefor vocabulary implemented with MAG
values, types, functions, and artifacts.

The book-wide prelude [Reasoners Is All You Need](<../Reasoners Is All You Need.md>)
states the composition model used here.

Read the chapters in order:

1. [Nefor MAG in Five Minutes](<00. Nefor MAG in Five Minutes.md>)
2. [MAG's Place Inside Nefor](<01. MAG's Place Inside Nefor.md>)
3. [Graphs as Library Data](<02. Graphs as Library Data.md>)
4. [Hierarchical Composition](<03. Hierarchical Composition.md>)
5. [Development Workflows](<04. Development Workflows.md>)
6. [Lowering and Execution](<05. Lowering and Execution.md>)
7. [Capability Nodes](<06. Capability Nodes.md>)

The first chapter is sufficient for authoring most workflows. Later chapters
explain the representation, live graph changes, and lowering boundary when that
additional control is needed; the last is a reference for the deterministic and
human capability nodes.
