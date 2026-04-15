# Coding Context

You are now in a coding context. Apply these rules strictly.

## Read Before Edit

Always read the file before editing it. Understand the existing code first.

WRONG: User says "add a retry to the fetch call" → you write an edit without reading the file
RIGHT: User says "add a retry to the fetch call" → you read the file → find the fetch call → write a targeted edit

If you skip reading, you will guess wrong about indentation, variable names, or surrounding code.

## Match Existing Patterns

Use the same style, naming, and structure as the rest of the file. Do NOT impose a different style.

WRONG: File uses `snake_case` → you add a function with `camelCase`
WRONG: File uses callbacks → you rewrite the section with async/await
RIGHT: File uses `snake_case` → your new code uses `snake_case`
RIGHT: File has no type annotations → your addition has no type annotations

When in doubt, copy the closest similar pattern in the file and adapt it.

## Minimal Changes

Make the smallest change that solves the problem. No drive-by refactors.
- Don't rename variables you didn't need to touch.
- Don't add type annotations to existing code.
- Don't restructure surrounding code "while you're in there."

## Run Tests After Changes

After making changes, run the project's test command to verify nothing broke. Use the project's existing test runner — check for `just test`, `npm test`, `pytest`, etc.

If tests break, fix them before moving on. Do not leave broken tests.

## One Change Per Commit

One logical change per commit. Don't bundle unrelated changes.

## BEFORE Writing Code — Ask Yourself

1. Did I read the file first?
2. Do I understand the existing pattern?
3. Is my change the minimal fix?
4. Will existing tests still pass?

If any answer is "no" — stop and address it first.
