# nefor-mag

A pure compile-time data-construction language for composing Nefor reasoner workflows. The lead is a "mage" (маг) casting reasoner compositions through algebraic notation.

## Pipeline

`.mag` source → lexer → parser → evaluator → graph validator → normalized JSON IR + sha256 hash → executor

## Language

MAG uses a Lisp-like syntax where code is data. Core constructs:

- `def`, `fn`, `let`, `if` — standard binding and control flow
- `->` — threading macro
- `node` — declare a typed graph node with a kernel factory, params, and type annotation
- `agent` — the tool-use loop template (`(agent {:id … :system … :provider …} : IN -> OUT)`); unbounded by default, `:max-steps N` opts into a loop-counter + exhaust summarizer; there is no "agent" node factory
- `graph` — compose nodes with directed edges
- `type` — forward-declare a type name
- `require` — load modules from the library path
- `read` — read a file as text, optionally interpolate `{key}` patterns
- `flat-map` — map a function over a collection and flatten the results

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

(node "llm" {:provider "chatgpt" :tools ["fs/read"]}
  : generic-provider.ProviderOut -> (generic-tool.ToolCalls | generic-provider.FinalAnswer))
```

Union types (`A | B`) create fanout nodes. The type names must match registered runtime combinators for fanout routing to work.

## Examples

See `examples/` for complete workflow definitions.
