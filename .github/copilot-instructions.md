# Copilot instructions

Read and follow [`AGENTS.md`](../AGENTS.md) before changing this repository.

The maintained implementation is Rust in `crates/contribai-rs`; `python/` is legacy reference
code. Preserve ContribAI's core invariant: analysis is the default, and every external write needs
explicit operator intent, repository consent, scoped admission, evidence, and human review. Never
add a CLA-signing flow or a publication bypass.

Before submitting changes, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
