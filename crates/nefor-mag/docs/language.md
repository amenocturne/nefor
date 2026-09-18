# MAG authoring reference

MAG is a small, typed, expression-oriented language. Programs compose immutable graph values from namespaced libraries. This reference covers the supported author-facing layer; runtime implementation structures are intentionally not public API.

See [Orchestrating MAG](orchestrating.md) for lead tools, [Patterns](../../../plugins/mag/docs/patterns.md) for common topologies, and [Errors](errors.md) for diagnostics.

## Modules and data

Imports are declarations, not runtime values. Each supported form has a distinct visibility effect:

```mag
import nefor.actors        // open every direct export and permit nefor.actors.name
import nefor.graph.{}      // qualified-only
import nefor.node.{named, rename as rename_node} // selected local names
import nefor.shell as shell // static namespace alias; only shell.name is permitted
import nefor.actors.{adapter_factory as `_`} // suppress an automatic spelling from an open import
```

Imports compose order-independently. Suppressions affect only automatic names from a bare import, not explicit selectors. Namespace roots, namespace aliases, builtins, local declarations, and opened or selected exports share one collision domain; collisions are errors, never source-order choices. Use qualified-only imports, narrower selectors, selector or namespace renames, or `as _` to repair them. `import m.*` is not syntax.

Qualified references require a matching direct import. Visibility is non-transitive: importing a module does not expose the dependencies that module imported. Every module evaluates once per compilation, circular imports are rejected, and only its direct top-level declarations are exports. An unresolved name remains an error even when diagnostics statically suggest sorted candidate imports; candidate discovery parses files without evaluating their bindings, dependencies, or file inputs.

A `.mag` suffix selects this syntax and a `.magl` suffix selects the legacy Lisp parser; explicit entry overrides may select either frontend without changing imported modules' suffix selection.

Declare nominal records and algebraic data types with `type`:

```mag
type Finding {path: String, summary: String}
type Decision = Finding(Finding) | AgentError(nefor.contracts.AgentError)
```

Algebraic alternatives are owned by their declared ADT. Construct one through the owner, such as `Decision.Finding(finding)` or `Decision.AgentError(failure)`. `(A, B)` is an anonymous all-of product type, not a nominal declaration. Product occurrences matter: `(T, T)` requires two matching incoming edges from distinct senders.

Alias-shaped declarations are transparent after generic substitution: `type Label = String` and `type Pair<T> = (T, T)` introduce alternate spellings but no semantic identity. Compatibility, inference, construction, type descriptors, schemas, and semantic IDs see the target type.

`newtype UserId = String` retains a distinct nominal identity with the target's runtime representation. There is no implicit conversion in either direction, including through a `let` annotation or function argument. Expression ascription is the explicit one-boundary operation: `(raw: UserId)` introduces the newtype and `(id: String)` eliminates it. Unrelated newtypes cannot be converted directly. Newtype descriptors and semantic IDs retain the qualified owner and differ from their target and from separately declared newtypes.

Eliminate an ADT with an exhaustive `match`. Each case names one constructor, binds its payload at that constructor's concrete type, and produces the same result type:

```mag
let describe: fn(Decision) -> String = |decision| => match decision {
  case Finding(finding) => get(finding, "summary"),
  case AgentError(failure) => canonical(get(failure, "last_output")),
}
```

A case has the shape `case Constructor(binding) => expression`. Missing, repeated, foreign, or non-nominal cases are rejected while checking. Evaluation selects the case from constructor evidence retained by MAG, never from a user-authored string field. Generic ADT instantiations retain their owner and constructor identities during exhaustiveness checking. Generic binder names must be unique within one binder list; nested scopes may reuse names.

Ordinary strings interpret `\n`, `\t`, `\\`, and `\"`. Triple-quoted strings are raw and may span lines; quotes, `$`, and backslashes inside them have no special meaning. <code>strip_margin</code> follows Scala's margin convention, removing leading whitespace through `|` while preserving line breaks:

```mag
let script = strip_margin("""|set -e
                               |echo 'export PATH="$HOME/.local/bin:$PATH"'
                               |find . \( -name '*.mag' -o -name '*.md' \)""")
let command = replace(script, "\n", " ")
```

## Native data collections and equality

`[...]` constructs the single native `List` representation. Parenthesized comma-separated values construct a product such as `(A, B, ...)`. A nominal record value uses `Type {field: value}`; a generic record uses `Type<A, B> {field: value}`. Braced fields without a nominal type name are blocks, not anonymous record values. Use `Map<K, V>` for homogeneous dynamically keyed data.

Native `Map<K, V>` and `Set<T>` values have no literal syntax. Import `core.map.{}` or `core.set.{}` and construct them through those ordinary modules. Map/Set lookup, membership, count, and insertion do not reveal storage order; no ordered fold or enumeration API is exposed. Inserting an equal existing key or member is an evaluation error. Map and Set equality is extensional and unordered.

`=` performs exact recursive equality over concrete data. It preserves nominal ADT ownership and constructor identity, compares products/lists positionally, and compares Float bit patterns. Functions, type witnesses/descriptors, packed compiler values, artifacts, and data containing such opaque behavior are rejected statically. Generic functions that use equality carry this requirement to each instantiation.

Artifact serialization is deterministic but does not make collection order observable. String-keyed maps become canonically keyed JSON objects; other maps and sets use reserved `$mag` envelopes whose entries are sorted only while serializing.

## Checked type evidence

`type_tag<T>()` produces a checked `TypeTag<T>` witness. `type_evidence(tag)`
returns its opaque `TypeDescriptor`, `type_id(descriptor)` its semantic identity,
and `type_schema(tag)` its validation schema. These low-level APIs retain their
witness arguments; ordinary library constructors such as `identity<T>(id)`
select types through explicit generic arguments instead.

Descriptor operations do not forge witnesses or cast values:

- `type_constructor(descriptor)` returns the qualified nominal owner name, or
  an empty string for a non-nominal type.
- `adt_constructor_payload(descriptor, name)` returns the payload descriptor for
  a constructor owned by an ADT and rejects non-ADT owners or unknown names.
- `type_arguments(descriptor)` returns the nominal owner's generic arguments,
  or an empty list for a non-nominal type.
- `type_components(descriptor)` returns immediate nested descriptors: nominal
  arguments followed by record fields (name order), a newtype's underlying
  type, or ADT payloads (constructor order); collection items, map key/value,
  and product components retain their structural order. Primitives have none.
- `list_type(descriptor)` constructs the descriptor of the ordinary native
  `List` type. `descriptor_schema(descriptor)` derives its validation schema.
  Neither operation converts a value into a runtime `DynamicList` protocol.

The internal relocation checker uses `packed_path_strings(packed, path, list)`
to inspect a record path containing a String or, when `list` is true, a
`List<String>`. Missing paths and wrong shapes fail during compilation; the
operation does not reinterpret arbitrary packed values as typed application data.

## Bindings and lexical blocks

Bindings use `let`, with an optional type annotation:

```mag
let prefix = "hello"
let message: String = str(prefix, " world")
```

A source file and every braced expression block are lexical blocks. Each direct `let name = value` declaration adds one immutable typed binding to that block; a block's final expression is its value. `let` is not valid as an ordinary argument, list item, or unbraced branch. Use a block or extract a helper function when an expression needs several declarations.

Peer declarations are mutually visible, so functions can call later functions and form mutually recursive families. Strict values remain eager: acyclic forward references are scheduled automatically, while an eager initialization cycle is rejected. A name denotes typed overloads, so the same spelling may be used for different semantic types but not twice for the same type.

Function types use `fn(A, B) -> C`; function values use `|a, b| => expression`. Generic parameters follow the binding name:

```mag
type Entry<K, V> {key: K, value: V}
let entry<K, V>: fn(K, V) -> Entry<K, V> = |key, value| =>
  Entry<K, V> {key: key, value: value}
```

A call without angle brackets infers every generic argument as before. An
immediate named call may instead supply the complete declared list, such as
`entry<String, Int>("answer", 42)`, and may put `_` in any position that should
still be inferred, such as `entry<String, _>("answer", 42)`. Once angle
brackets are present, every declared generic position must appear: trailing
omission is not shorthand. Explicit call arguments participate in overload
selection and work through qualified module names and import aliases. The form
specializes only that immediate call; it does not create a specialized
function value.

Ordinary word identifiers use snake_case without quoting. Backticks are reserved for names that genuinely require escaping, such as the qualified symbolic operator <code>nefor.node.`>>>`</code> or the reserved field name <code>`type`</code>.

Symbolic operators may be used as ordinary values and called with parentheses, or declared with a fixity and applied infix. `left op right` is the ordinary binary call `(op)(left, right)`, not a curried application. Infix application requires ASCII whitespace on both sides of a symbolic operator: write `left >>> right`, never `left>>>right`, `left >>>right`, or `(left)>>> right`. Spaces, tabs, and line breaks are separators; delimiters are not. A line may continue when a newline follows the operator (`left >>>` with `right` on the next line), while an operator at the start of the next line begins a new expression and is rejected. A `//` comment does not replace the required whitespace: write a space before the comment in `left >>> // explanation`. Alphabetic infix names use the same expression-separated position (`1 add 2`) without a separate symbolic-spacing check. This rule does not affect prefix calls such as `(>>>)(left, right)`, qualified calls such as <code>nefor.node.`>>>`(left, right)</code>, `->` in types, lambda delimiters, or signed numeric literals.

## A complete graph

```mag
import core.types.{}
import agents.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}

type InspectionInput {prompt: String}

let start = nefor.graph.source("task", InspectionInput {prompt: "Inspect the repository."})
let worker = agents.agent<InspectionInput, nefor.contracts.TextAnswer>("worker", agents.AgentConfig {model: agents.standard, system: "Inspect the repository and report the result.", tools: nefor.actors.read_only_tools, tool_approval_policy: named(nefor.contracts.ToolApprovalPolicy, Default, nil), max_corrections: 2})
let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")

nefor.artifact.compile((|graph| => nefor.graph.add_edges(graph, [
  nefor.graph.edge(start, worker),
  nefor.graph.edge(worker, result),
])): fn(nefor.graph.Graph) -> nefor.graph.Graph)
```

An authored program is a pure `Graph -> Graph` function. `nefor.artifact.compile` applies it to <code>nefor.graph.empty_graph</code>, validates the complete topology, and prepares a fresh run. Build one flat edge list; compose edge families with `concat` and `map` rather than nested lists.

### Sources and output

`nefor.graph.source<T>` captures and emits a value checked against `T`. More generally, any exposed node whose complete input type is exactly `Unit` may be an unfed root and receives one automatic activation; feeding it through an edge suppresses that activation. An ADT or product that merely contains `Unit` still requires an explicit input.

`nefor.graph.output<T>` is a concrete `T -> T` identity node and the result boundary. A graph must contain exactly one, it must be terminal, and every ordinary node must be reachable from a root and able to reach it. <code>nefor.graph.output_for</code> derives the compatible type from a preceding node.

### Semantic types and runtime wires

A port stores compiler-checked semantic evidence and a runtime protocol wire. Public constructors derive the latter internally:

- Any input type other than `nefor.contracts.ProviderInput` starts a fresh typed user turn.
- `nefor.contracts.ProviderInput` is the nominal continuation type and passes an already-built provider turn through unchanged.

MAG programs provide only `type_tag<T>()` and connect compatible typed ports. They do not name, invent, or construct the runtime wire protocol. Prefer public constructors such as `source`, `agent`, `output`, `process.exec`, and `worktree.create`.

## Agents

`nefor.actors.agent` has semantic type:

```text
I -> core.types.Result<nefor.contracts.AgentError, O>
```

The whole result must be handled by a compatible downstream node or terminal output. `AgentError` preserves `last_output` and classifies the reason as provider failure or structured-output validation failure. The structured agent automatically asks the model to correct invalid output up to `max_corrections`; `0` means only the initial attempt. Exhaustion emits `AgentError` as data—it is not a successful `O` and should be routed deliberately.

`tools` is the agent's capability boundary. Use <code>nefor.actors.read_only_tools</code>, <code>nefor.actors.general_tools</code>, or an explicit list. A tool call not in the invocation allowlist is rejected even if the tool exists globally. `tool_approval_policy` configures command policy; it does not replace the runtime approval gate.

If a downstream reviewer can work with partial failed output, accept the full result as input. Otherwise bind the successful continuation with <code>nefor.result.`>=>`</code>, consume the full `Result`, or use an explicit Result unpack/repack node. `nefor.node.choose` is the corresponding branching combinator for `core.types.Either<A, B>`.

## Edges, products, ADTs, and joins

`nefor.graph.edge` connects compatible nodes. A producer may fan out through several edges; a consumer may fan in through several edges.

- A single input fires for each matching arrival.
- A nominal ADT input is one owner type and fires for each complete owner value; constructors are unpacked only by explicit branching nodes.
- A product input `(A, B)` fires only after every product occurrence is filled. This is the all-of join mechanism.
- Multiple edges from one producer express fan-out.
- A `Unit` dependency edge expresses ordering without transferring domain data.
- Cycles are legal if all nodes remain source-reachable and output-reachable.

There is no graph mutation API. `graph`, <code>add_edges</code>, and <code>remove_edges</code> are total pure set operations over the graph being authored.

## Process and shell nodes

Use structured `process.exec` when one executable plus arguments expresses the operation:

```mag
import nefor.contracts.{}
import nefor.process.{}

nefor.process.exec("search", nefor.process.ProcessExecParams {
  argv: ["rg", "-n", "TODO", "src/"],
  cwd: nefor.process.cwd,
  timeout: named(nefor.contracts.Timeout, Unlimited, nil),
})
```

`argv` is passed directly to process spawn: no shell is inserted, so operators such as `|`, `>`, glob expansion, and shell built-ins are ordinary arguments. For shell syntax, use explicit POSIX `shell.script`, which lowers to `["/bin/sh", "-c", script]`:

```mag
import nefor.contracts.{}
import nefor.shell.{}

nefor.shell.script("bounded-search", nefor.shell.ShellScriptParams {
  script: strip_margin("""|rg -n 'TODO|FIXME' src/
                            |  | sort"""),
  cwd: ".",
  timeout: named(nefor.contracts.Timeout, Seconds, 30),
})
```

POSIX shell does not imply Bash. When Bash semantics are required, invoke it explicitly with `process.exec`, for example `argv: ["/bin/bash", "-lc", "set -o pipefail; command"]`.

Both nodes require a non-empty `cwd`; relative paths resolve from the MAG host's inherited working directory, exposed as `nefor.process.cwd` (`"."`). They accept `Unit`; an unfed node receives one automatic activation, while an incoming `Unit` edge makes it dependency-driven. The output is `ProcessResult`, containing separate `stdout`, `stderr`, and a nominal `ProcessExited` or `ProcessSignaled` termination value. Use exhaustive `match` to distinguish the two constructors; authored MAG never compares process-termination strings. Nonzero exit is result data, not a compilation failure.

Timeouts are mandatory and explicit. <code>named(nefor.contracts.Timeout, Unlimited, nil)</code> is unbounded; use it only when waiting indefinitely is intentional. <code>named(nefor.contracts.Timeout, Milliseconds, N)</code> sets a positive wall-clock bound. `named(Timeout, Seconds, N)` and `named(Timeout, Minutes, N)` normalize through the same checked signed-integer conversion. Zero, negative, and overflowing durations fail when constructing a node during compilation. A process that never exits keeps its run nonterminal, so an awaited run also waits indefinitely. The current API has no `bash`, `BashOptions`, `command-with-options`, or `pipe-command` compatibility surface.

## Human approvals

<code>nefor.human.approval_gate</code> turns a `TextAnswer` into `nefor.human.HumanWorkflowDecision`, with `Approved(HumanWorkflowApproval {content})` and `Rejected(HumanWorkflowRejection {reason})` alternatives. Use it when human judgment is part of the graph's meaning. It is distinct from lead `write-review`, which authorizes execution of a write-capable orchestration plan before launch. See [Orchestrating MAG](orchestrating.md#author-and-launch-a-program).

## Runtime expansion

Most workflows should be fully static. When runtime data determines cardinality, a producer exposes the indexed-items-plus-completion `DynamicList<T>` protocol. Consumers retain that same nominal boundary: <code>nefor.dynamic.traverse</code> materializes one worker per item, while `nefor.dynamic.context` buffers through completion and activates once with the ordered list. The operation or factory owns this interpretation; the compiler grants no privileges from type-name spelling. Fixed worker lists use `nefor.node.sequence`.

Version 1 evaluates only the closed Trigger, Capture, Field, IntToDecimalString, and ConcatStrings expression forms while materializing a structural delta template. There is no general runtime expression language or post-compilation MAG function application. Operations are program metadata, not graph edges, and do not give actors authority to alter the graph. See [MAG composition semantics](../../../plugins/mag/docs/patterns.md).

## Worktrees

`nefor.worktree.create` and `nefor.worktree.open` are explicit, typed workflow nodes:

- `create` requires absolute repository and worktree paths plus branch and base. It creates only a fresh branch/worktree and refuses to adopt an existing path or local branch.
- `open` validates an existing repository/path/branch triple and never creates or changes it.

Successful worktrees outlive the MAG run. The public capability intentionally has no merge, removal, inventory, or cleanup operation. Route the returned `Worktree` into agents that need the isolated path, and keep integration or cleanup outside the graph unless an explicit capability owns it.
