# CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

---

## 5. Commits

**Use short Conventional Commit messages.**

- Format commit subjects as `type(scope): summary` where the scope is useful.
- Keep the summary short and concrete.
- Do not add long commit descriptions unless the change genuinely needs one.

---

## 6. Rust-Specific Guidelines

**Use the type system. Keep functions small. Make business logic read like pseudocode.**

### Types over strings and `Value`

- Replace stringly-typed parameters (`&str` for chains, IDs, hex bytes) with enums and newtypes.
- Replace `serde_json::Value` in business logic with `#[derive(Deserialize)]` structs. `Value` is a parsing-boundary tool; once data is in the program, it has a shape — give it one.
- Make illegal states unrepresentable. If a field is required for one variant and absent for another, that's an enum, not an `Option`.

### Function shape

- One function does one thing. The name describes that thing exactly. If you need a comment to explain *what* it does, the name is wrong.
- Extract for readability *even when not reused*. A 30-line block with a clear name beats a 30-line inline block.
- Ceilings (enforced by clippy, not vibes): cognitive complexity ≤ 25, lines ≤ 100, arguments ≤ 7.
- More than 7 arguments → group them in a struct (e.g., `PollPipelineArgs`). The struct doc replaces the argument-list comments.

### Owned types in bundle structs

When grouping arguments into a struct, prefer owned types (`String`, `Vec<u8>`, `T`) over borrowed types (`&str`, `&[u8]`, `&T`).

Lifetime parameters force every consumer to thread `'a` through their own signatures and structs — that cognitive overhead is almost never worth the saved allocation in this codebase's call patterns (a few transactions per run, small payloads). The struct should read like pseudocode at the call site: `SuiGmpCall { destination_chain: "flow".into(), payload, gas_value_mist: 0, gas_budget_mist: 0 }` — no `<'a>`, no lifetime juggling.

Exceptions:

- Mutable references (`&mut Vec<...>`, `&mut Receiver<...>`) cannot be owned without moving them out of the caller. Pass these as separate arguments alongside an owned bundle struct, not as fields *in* the bundle. Example: `poll_pipeline(txs: &mut Vec<PendingTx>, rx: Option<&mut Receiver<PendingTx>>, args: PollPipelineArgs)`.
- A struct that's genuinely a view into caller-owned data on a measured hot path (rare in this codebase).
- `&'static str` (compile-time literals) is fine — that's not a lifetime parameter on the struct.

If you find yourself reaching for `<'a>` on a bundle struct, ask: is the alloc actually measurable? If not, take the alloc and let the struct be lifetime-free.

### Business logic reads like pseudocode

- The top-level orchestrator (`run`, `verify_onchain`, `relay_to_destination`) should be a sequence of named calls — a reader scanning it understands the feature without going deeper.
- Push details (RPC parsing, retry loops, byte slicing) into helpers named for what they produce, not how.
- When a function exceeds ~100 lines, that's a signal it's holding multiple responsibilities — split before adding more.

### Module layout

- Types live in their own modules. A file mixing `pub struct Foo`, `impl Foo`, and `pub async fn business_workflow(...)` is doing two jobs.
- Pattern: `feature/{types.rs, helpers.rs, mod.rs}` where `mod.rs` is the orchestrator and the others hold reusable building blocks. Keep file scope narrow.
- Imports always at the top of the module; never `use` inside a function body.

### Deterministic over LLM

- If a rule can be expressed as a lint or a hook, do that — don't rely on review or memory to catch it.
- Before adding a guideline here, ask: "Can clippy/rustfmt/a git hook enforce this?" If yes, configure the tool. CLAUDE.md is for what the tooling can't catch.
- Existing deterministic gates: `[lints.clippy]` in `Cargo.toml`, `clippy.toml` (`disallowed-methods` blocking the solana-axelar crates' compile-time IDs), `.githooks/pre-commit` and `.githooks/pre-push` (fmt + clippy + tests). Extend these in preference to writing more prose.

### Before reporting an edit as done

After any Rust edit, run both of these and resolve every diagnostic before handing back:

- `cargo fmt --all --check`
- `cargo clippy --all-targets -- -D warnings -A clippy::too_many_lines`

The `-A clippy::too_many_lines` flag is **mandatory** — it matches the project gate in `.githooks/pre-push`. The repo intentionally keeps `too_many_lines = "warn"` in `Cargo.toml` for visibility, but does not deny on it (see the inline rationale next to the lint). Running plain `cargo clippy ... -D warnings` will report ~60 pre-existing too-many-lines warnings on orchestrators that are large by design — those are not your bug to fix.

A passing `cargo check` is not sufficient; the clippy invocation above is the bar.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
