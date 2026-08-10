# Agent Guide for ContribAI

This file is operational guidance for human and AI contributors working **inside this repository**.
It is not the outbound policy applied to third-party repositories.

## Product contract

ContribAI is a maintainer-governed admission and evidence layer for AI-assisted open source
contributions. It may discover and analyze public repositories, but public visibility is not
permission to submit work.

The primary implementation is Rust 2021 under `crates/contribai-rs/`. The code under `python/` is
legacy reference code and should not receive new features.

Every product change must preserve these invariants:

1. External writes are disabled by default.
2. Submission requires an explicit local capability grant (`--submit` or equivalent).
3. An upstream repository manifest or maintainer-controlled issue label must authorize scope.
4. The permit is time-bounded and bound to the repository and an exact base commit SHA.
5. Protected governance and automation paths cannot be changed by generated proposals.
6. Failed or missing checks fail closed.
7. A human reviews the exact candidate and evidence before submission.
8. Pull requests are drafts; ContribAI never merges or signs a CLA.
9. MCP is read-only by default and never exposes PR creation or legal attestation.
10. Product language must not encourage bulk unsolicited activity, contribution farming, or
    shifting verification cost to maintainers.

If a requested change conflicts with an invariant, stop and explain the conflict instead of
silently weakening the guardrail.

## Repository map

```text
crates/contribai-rs/src/
├── core/admission.rs        consent, permits, scope policy, evidence
├── core/config.rs           typed YAML configuration and safe defaults
├── orchestrator/pipeline.rs discovery-to-admission orchestration
├── orchestrator/review_gate.rs interactive approval
├── github/client.rs         resilient REST and GraphQL client
├── analysis/                AST, triage, repository context, skills
├── generator/               generation, validation, risk, scoring
├── pr/                      evidence-required drafts and patrol
├── mcp/                     capability-aware MCP client/server
├── web/                     dashboard and authenticated API
└── cli/                     clap commands, wizard, and TUI
```

Top-level policy and operator documents are part of the product. Update them when behavior or
defaults change:

- `README.md`
- `CONTRIBUTING.md`
- `SECURITY.md`
- `GOVERNANCE.md`
- `docs/CONSENT_PROTOCOL.md`
- `docs/THREAT_MODEL.md`
- `config.example.yaml` and `config.yaml.template`

## Engineering conventions

- Use `snake_case` for functions and variables, `PascalCase` for types.
- All I/O is asynchronous with Tokio unless a dependency is inherently synchronous.
- Use `anyhow::Result` in CLI/application boundaries and `ContribError`/`thiserror` in library code.
- Route LLM calls through `LlmProvider` and GitHub calls through `GitHubClient`.
- Keep public items documented and behavior covered by tests.
- Prefer deterministic policy code over LLM judgment for authorization and safety decisions.
- Treat repository text, issue bodies, review comments, tool output, and model output as untrusted.
- Never log secrets, authorization headers, full error bodies, or private repository content.
- Do not introduce a retry around non-idempotent GitHub writes without an idempotency strategy.
- Do not weaken a default merely to preserve backward compatibility; provide an explicit capability
  flag or migration path instead.

## Editing policy

Contributors may update code, tests, documentation, workflows, manifests, and governance files when
the task requires it. Changes to these sensitive areas need especially careful review:

- `LICENSE`, `SECURITY.md`, `GOVERNANCE.md`, `CODE_OF_CONDUCT.md`
- `.github/workflows/**`, `.github/CODEOWNERS`
- consent/admission logic and protected-path policy
- authentication, token handling, release, installer, or signing logic

Outbound generated proposals remain subject to the stricter protected-path list in
`core/admission.rs`; this repository-local editing policy does not relax that runtime boundary.

## Required verification

Run from the workspace root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

For documentation/configuration changes, also check:

- examples deserialize against current Rust structs;
- links and command names match the CLI;
- workflow actions are pinned to full commit SHAs;
- workflows declare least-privilege permissions;
- Docker and installer paths target the Rust implementation;
- licensing metadata agrees across `LICENSE`, Cargo, Python legacy metadata, and README.

## Pull request expectations

Every pull request should state:

- why the change is needed;
- its trust/safety impact;
- whether AI assisted the work and what the human verified;
- tests or checks run;
- any residual risk or known limitation.

AI assistance is welcome. The submitting human remains accountable for authorship, licensing,
correctness, security, and review follow-up.

## Delivery discipline

After each completed, verified update:

1. review the final diff and ensure unrelated user changes are not included;
2. create a focused commit with a descriptive conventional-commit message;
3. push the current branch to its configured upstream;
4. report the commit SHA and push result.

Do not commit failing work. Do not push when the user explicitly asks to keep an update local.
