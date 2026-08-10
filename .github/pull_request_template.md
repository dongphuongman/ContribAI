## Why

<!-- What maintainer/user problem does this solve? Link an issue when available. -->

## What changed

<!-- Keep this concrete and scoped. -->

## Trust and safety impact

- [ ] No external-write, authentication, credential, sandbox, workflow, or release behavior changed
- [ ] Safety-sensitive behavior changed and the threat model / deny-path tests were updated

Explain any new capability, permission, network destination, persisted data, or residual risk:

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo build --workspace --release`
- [ ] User-facing docs/config examples match the implementation

Commands and notable results:

```text

```

## AI assistance

- [ ] No meaningful AI assistance
- [ ] AI assisted this change; I reviewed every changed file and remain accountable for it

If assisted, name the tool/model if known and summarize what the human verified:

## Checklist

- [ ] The change is focused and does not reward unsolicited contribution volume
- [ ] New GitHub writes are explicit, consented, bounded, and fail closed
- [ ] No bot or agent performs a legal attestation for a person
- [ ] No secrets, private code, or personal data were added
- [ ] Dependencies and copied material are license-compatible
- [ ] Breaking behavior and migrations are documented

## Related issues

<!-- Closes #123 -->
