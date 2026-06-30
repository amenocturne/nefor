# nefor-mag

A pure compile-time data-construction language for composing Nefor reasoner workflows. The lead is a "mage" (маг) casting reasoner compositions through algebraic notation.

## Pipeline

`.mag` source → lexer → parser → evaluator → graph validator → normalized JSON IR + sha256 hash → executor

## Language

MAG uses a Lisp-like syntax where code is data. Core constructs:

- `def`, `fn`, `let`, `if` — standard binding and control flow
- `->` — threading macro
- `node` — declare a typed graph node with reasoner, args, and type annotation
- `graph` — compose nodes with directed edges
- `type` — forward-declare a type name
- `require` — load modules from the library path
- `template` — read and interpolate template files

## Type System

Types annotate node inputs and outputs for graph validation and fanout routing:

```lisp
;; Bare types get a `mag.` prefix — use for graph-internal routing
(type Findings)
(type Summary)

;; Qualified types pass through — use for runtime combinator matching
(type generic-provider.ProviderOut)
(type generic-tool.ToolCalls)
(type generic-provider.FinalAnswer)

(node "agent" {:tools ["fs/read"]}
  : generic-provider.ProviderOut -> (generic-tool.ToolCalls | generic-provider.FinalAnswer))
```

Union types (`A | B`) create fanout nodes. The type names must match registered runtime combinators for fanout routing to work.

## Examples

See `examples/` for complete workflow definitions.
