# ContribAI architecture

ContribAI is a maintainer-governed contribution agent. Its default product is a local analysis
plan. Publishing to somebody else's repository is a privileged, multi-gate operation.

## Trust boundaries

```text
untrusted repository + issue text
              │
              ▼
 discovery → analysis → candidate generation → local validation
                                              │
                                              ▼
                       admission + evidence + human review
                                              │
                                              ▼
                              draft pull request (optional)
```

Repository content, issue bodies, generated patches, tool output, and LLM output are untrusted.
GitHub and LLM credentials are authority-bearing secrets. The external-write boundary starts at
branch, issue, comment, reaction, and pull-request creation.

## Runtime

The Rust workspace in `crates/contribai-rs` is the maintained implementation. `python/` is a
legacy reference implementation.

- `cli/` owns explicit operator intent. Write-capable commands require `--submit`; patrol replies
  require `--respond`; the MCP server requires `--allow-writes`.
- `orchestrator/pipeline.rs` coordinates discovery, analysis, generation, validation, admission,
  review, and optional publication.
- `core/admission.rs` validates repository consent, issue-scoped approval, base revision,
  protected paths, change budgets, and evidence.
- `analysis/` treats repository text as data and builds bounded code context.
- `generator/` creates and scores candidate changes but has no independent publication authority.
- `pr/manager.rs` recomputes evidence, revalidates live maintainer consent, and creates draft pull
  requests only.
- `github/client.rs` centralizes GitHub I/O, retry classification, rate limits, and exact-SHA branch
  creation.
- `orchestrator/memory.rs` stores local outcomes, short-lived working context, and integrity-linked
  admission decision receipts in SQLite.
- `web/` is an observability API. It does not claim to queue runs; public binds require an API key.
- `mcp/` exposes tools over stdio with read-only defaults.
- `cli/commands/demo.rs` exercises the production consent, admission, and evidence policy against
  bundled values without loading configuration, credentials, network clients, or write capability.
- `site/` is a static public onboarding surface, not part of the runtime trust boundary. It has no
  backend, credentials, analytics, cookies, or write capability.

## Publication protocol

Every external contribution must satisfy all of these conditions:

1. The operator explicitly enables external writes.
2. The repository opts in through `.github/contribai.yml`, or a maintainer-approved issue provides
   scoped consent.
3. The target base commit SHA is captured before generation.
4. Paths and patch size fit the repository's declared scope and built-in protected-path rules.
5. Required validation checks pass and are recorded in an expiring `EvidenceCapsule`.
6. A human reviews every proposed byte and approves the exact candidate interactively.
7. The terminal decision is appended to the local audit ledger; an approval that cannot be recorded
   fails closed.
8. Evidence and live maintainer consent are revalidated at the write boundary.
9. The branch is created from the attested SHA and the pull request is opened as a draft.

Blocked attempts are recorded at the capability, permission, consent, base-revision, evidence, or
admission boundary. Human rejection, skip, approval, and review errors are recorded separately. The
ledger contains hashes and scope metadata rather than generated file contents. Its linked receipts
detect accidental local mutation and ordering breaks, but are not signatures and do not protect
against an attacker who can replace the entire database and every external checkpoint.

There is intentionally no pipeline API that pre-approves the human gate. ContribAI never signs a
CLA on behalf of a person. Details are in [docs/CONSENT_PROTOCOL.md](docs/CONSENT_PROTOCOL.md) and
[docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).

## Failure model

The safe result of ambiguity, missing consent, stale base state, failed evidence, unavailable
review, exhausted quota, or an authentication error is no external write. Retries are bounded and
limited to transient failures. Partial local analysis can still be returned to the operator.

## Design constraints

- Single-repository changes only.
- Generated changes are code-focused; governance, funding, security policy, and license paths are
  protected from target-repository automation.
- Scheduled execution is disabled by default.
- Configuration defaults to `pipeline.dry_run: true` and `pipeline.agent_mode: plan`.
- High-risk changes require explicit approval in addition to the normal gates.

See [AGENTS.md](AGENTS.md) for implementation invariants and [SECURITY.md](SECURITY.md) for the
supported security boundary.
