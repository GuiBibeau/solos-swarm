# Rust style

See also: [README](../README.md), [CHANGELOG](../CHANGELOG.md) (adopted in #13) and the Rust commands CI runs (`cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`).

Google publishes no single Rust style guide of its own. What exists, and what we follow:

- Default `rustfmt` style. Chromium states it follows the Rust Style Guide, the Rust API
  Guidelines and formats with a `.rustfmt.toml`
  (<https://chromium.googlesource.com/chromium/src/+/main/styleguide/rust/rust.md>).
  Fuchsia and Android also use stock rustfmt.
- Comprehensive Rust, "Idiomatic Rust" chapters (<https://google.github.io/comprehensive-rust/idiomatic/welcome.html>):
  clear call sites, standard vocabulary and naming, newtypes and "parse, don't validate",
  owned/view type pairs, no hidden clones, error context added across boundaries
  (`thiserror`/`anyhow`), meaningful doc comments.
- Comprehensive Rust, unsafe chapter (<https://google.github.io/comprehensive-rust/unsafe-rust/unsafe.html>):
  unsafe code is small, isolated and its correctness carefully documented.
- Rust API Guidelines (<https://rust-lang.github.io/api-guidelines/>) and the Rust Style Guide
  (<https://doc.rust-lang.org/style-guide/>).

## Enforced here

| Rule | Mechanism |
|---|---|
| Default rustfmt style | `rustfmt.toml` (edition only), `cargo fmt --all -- --check` in CI |
| clippy default set, no warnings | `[lints.clippy] all`, `cargo clippy --locked --all-targets -- -D warnings` in CI |
| Every `unsafe` block carries a `// SAFETY:` comment | `undocumented_unsafe_blocks`, `missing_safety_doc` |
| `unsafe` ops inside `unsafe fn` are explicit | `unsafe_op_in_unsafe_fn` |
| No `dbg!`, `todo!`, `unimplemented!` left behind | `dbg_macro`, `todo`, `unimplemented` |
| No glob imports (test `use super::*` is exempt by default) | `wildcard_imports` |

## Not adopted yet (follow-ups, counts at adoption)

| Lint | Why deferred |
|---|---|
| `unwrap_used`, `expect_used` | about 110 hits in non-test code (kernel, ledger, qa, runtime, scheduler); each needs a decision on panic vs error, and some sit on the ledger/hold path. `clippy.toml` already exempts tests. |
| `enum_glob_use` | 5 `use Enum::*` in qa.rs and sandbox.rs; large match arms to requalify. |
| `must_use_candidate` | about 150 hits, noisy for a binary crate; API-surface decision. |
| `cast_possible_truncation`, `cast_sign_loss`, `cast_possible_wrap` | about 70 `as` casts, many in timing and accounting code; each needs a checked conversion and a test. |
| `missing_errors_doc`, `missing_panics_doc`, `missing_docs` | about 300 and 23 hits; the crate is a binary, not a published API. |
| `print_stdout` | CLI output is intentional. |
| Glob/line-width overrides, nightly rustfmt options | Google uses stable defaults; we do the same. |
