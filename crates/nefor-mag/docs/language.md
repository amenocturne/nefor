# MAG language reference

MAG is a small, typed, expression-oriented language. A program evaluates pure, immutable values from namespaced modules and returns one artifact. This reference covers the supported author-facing language; compiler implementation structures are intentionally not public API. What an artifact means is decided by the application that embeds MAG, not by the language.

See [Compiling MAG](compiling.md) for the standalone compiler, [Errors](errors.md) for diagnostics, and the [core part of the MAG Book](<../../../mag/book/01. core/README.md>) for a guided introduction.

## Modules and data

Imports are declarations, not runtime values. Each supported form has a distinct visibility effect:

```mag
import core.types        // open every direct export and permit core.types.name
import core.map.{}        // qualified-only
import core.validated.{valid, invalid as reject} // selected local names
import core.set as sets   // static namespace alias; only sets.name is permitted
import core.types.{some as `_`} // suppress an automatic spelling from an open import
```

Imports compose order-independently. Suppressions affect only automatic names from a bare import, not explicit selectors. Namespace roots, namespace aliases, builtins, local declarations, and opened or selected exports share one collision domain; collisions are errors, never source-order choices. Use qualified-only imports, narrower selectors, selector or namespace renames, or `as _` to repair them. `import m.*` is not syntax.

Qualified references require a matching direct import. Visibility is non-transitive: importing a module does not expose the dependencies that module imported. Every module evaluates once per compilation, circular imports are rejected, and only its direct top-level declarations are exports. An unresolved name remains an error even when diagnostics statically suggest sorted candidate imports; candidate discovery parses files without evaluating their bindings, dependencies, or file inputs.

A `.mag` suffix selects this syntax and a `.magl` suffix selects the legacy Lisp parser; explicit entry overrides may select either frontend without changing imported modules' suffix selection.

Declare nominal records and algebraic data types with `type`:

```mag
type Finding {path: String, summary: String}
type Failure {reason: String}
type Decision = Finding(Finding) | Failure(Failure)
```

Algebraic alternatives are owned by their declared ADT. Construct one through the owner, such as `Decision.Finding(finding)` or `Decision.Failure(failure)`; a payload is never accepted as though it were the owner value.

`Owner.Constructor(arguments)` is the only construction form. The owner is resolved as an ordinary type name after imports, so a local, imported, qualified, namespace-aliased, or transparently aliased name all work, in any expression position and without an annotation. A generic owner's type arguments may be written, `Result<String, Int>.Ok(42)`, or omitted and inferred as for a generic call: from the payload (`Option.Some(42)`), from the expected type where one exists (an annotation, parameter, record field, function result, or established list element type), or from both together. A parameter that neither determines is an error that asks for the explicit form; a disagreement between payload and expected type is a type error. No arguments construct a `Unit` payload (`Flag.Off()`), one argument is the payload, several arguments form the constructor's tuple payload, and `name: value` arguments (or `Owner.Constructor {name: value}`) form its inline record payload.

Constructors are scoped by their ADT rather than by the module. A bare `Constructor(...)` is an ordinary call and never constructs, two ADTs in one module may declare the same constructor name, and a constructor never collides with a function, import, or type of the same name. Constructor names are unique within one ADT. A constructor is not a function value: `Owner.Constructor` must be applied. `(A, B)` is an anonymous all-of product type, not a nominal declaration. Product components are positional, so `(T, T)` has two distinct occurrences of `T`.

Alias-shaped declarations are transparent after generic substitution: `type Label = String` and `type Pair<T> = (T, T)` introduce alternate spellings but no semantic identity. Compatibility, inference, construction, type descriptors, schemas, and semantic IDs see the target type.

`newtype UserId = String` retains a distinct nominal identity with the target's runtime representation. There is no implicit conversion in either direction, including through a `let` annotation or function argument. Expression ascription is the explicit one-boundary operation: `(raw: UserId)` introduces the newtype and `(id: String)` eliminates it. Unrelated newtypes cannot be converted directly. Newtype descriptors and semantic IDs retain the qualified owner and differ from their target and from separately declared newtypes.

Eliminate an ADT with an exhaustive `match`. Each case names one constructor, binds its payload at that constructor's concrete type, and produces the same result type:

```mag
let describe: fn(Decision) -> String = |decision| => match decision {
  case Finding(finding) => get(finding, "summary"),
  case Failure(failure) => get(failure, "reason"),
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
  Neither operation converts a value.

`packed_path_strings(packed, path, list)` inspects a record path inside a
compiler-packed value: the path must hold a String or, when `list` is true, a
`List<String>`. Missing paths and wrong shapes fail during compilation; the
operation does not reinterpret arbitrary packed values as typed application data.
Packed values cannot be forged by building a map with matching fields; only the
operations that produce them do so.

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

Ordinary word identifiers use snake_case without quoting. Backticks are reserved for names that genuinely require escaping, such as a module-qualified symbolic operator like <code>m.`>>>`</code> or the reserved field name <code>`type`</code>.

Symbolic operators may be used as ordinary values and called with parentheses, or declared with a fixity and applied infix. `left op right` is the ordinary binary call `(op)(left, right)`, not a curried application. Infix application requires ASCII whitespace on both sides of a symbolic operator: write `left >>> right`, never `left>>>right`, `left >>>right`, or `(left)>>> right`. Spaces, tabs, and line breaks are separators; delimiters are not. A line may continue when a newline follows the operator (`left >>>` with `right` on the next line), while an operator at the start of the next line begins a new expression and is rejected. A `//` comment does not replace the required whitespace: write a space before the comment in `left >>> // explanation`. Alphabetic infix names use the same expression-separated position (`1 add 2`) without a separate symbolic-spacing check. This rule does not affect prefix calls such as `(>>>)(left, right)`, qualified calls such as <code>m.`>>>`(left, right)</code>, `->` in types, lambda delimiters, or signed numeric literals.
