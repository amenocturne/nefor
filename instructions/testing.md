# Testing Context

You are now working on tests. Apply these rules:

- Test behavior, not implementation. Tests should survive refactors.
- One assertion per test when possible. Name tests after the behavior being verified.
- Use the project's existing test framework and patterns — don't introduce new ones.
- Cover the happy path first, then edge cases that actually matter.
- Don't test trivial code (getters, simple mappings). Test logic and integration points.
- If a test is hard to write, the code probably needs a better interface.
