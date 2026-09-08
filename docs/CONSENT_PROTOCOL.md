# ContribAI Consent Protocol

Status: experimental. Consent manifest schema: 1. Evidence capsule schema: 2.

The protocol separates permission to read public code from permission to create an external
proposal. Consent is explicit, narrow, revocable, and evaluated again immediately before the write
workflow begins.

## Repository manifest

ContribAI checks these paths in order:

1. `.github/contribai.yml` (canonical)
2. `.github/CONTRIBAI_ALLOW` (legacy)
3. `CONTRIBAI_ALLOW` (legacy)

The format is intentionally small and line-oriented:

```yaml
schema_version: 1
enabled: true
max_files: 5
max_changed_lines: 250
allowed_paths:
  - src/**
  - tests/**
```

### Fields

| Field | Required | Meaning |
|---|---:|---|
| `schema_version` | no | If present, must be exactly `1` |
| `enabled` | yes | Must be exactly `true`; absence or `false` denies submission |
| `max_files` | no | Maximum changed and test files; default `5` |
| `max_changed_lines` | no | Approximate added-plus-removed line budget; default `250` |
| `allowed_paths` | no | YAML list or comma-separated glob allowlist; empty means any non-protected code path |

Unknown fields, unsupported schema versions, invalid positive integers, and invalid path patterns
invalidate the manifest. Strict parsing prevents a misspelled policy field from silently widening
scope. Consent files never authorize changes to protected governance or automation paths.

Allowed paths and generated file paths use canonical repository-relative POSIX syntax. Absolute
paths, backslashes, empty components, `.`/`..` components, URI metacharacters, invalid globs,
duplicate targets, and file deletions are denied. Literal glob separators are enforced, so `src/*`
does not authorize `src/nested/file.rs`; use `src/**` for recursive scope.

Removing the manifest or changing `enabled` to `false` revokes repository consent for future
permits. Existing draft pull requests remain visible and can be closed normally.

## Issue-scoped consent

A maintainer can approve a single issue by applying one of:

- `contribai-approved` (canonical)
- `agent-ready` (compatibility alias)
- `ai-contribution-approved` (compatibility alias)

GitHub normally restricts label application to users with repository triage permission or above.
ContribAI treats the label as intent to receive a proposal for that issue, not as acceptance of the
result. Removing the label or closing the issue prevents future permits.

## ContributionPermit

After consent is found, ContribAI issues a local permit containing:

- repository identity;
- exact base commit SHA;
- consent source;
- optional issue number;
- path and size budget;
- issue and expiry timestamps;
- draft-only requirement;
- deterministic permit identifier.

Permits expire after 24 hours. The base revision must be a full 40- or 64-hex Git object ID; an
abbreviated SHA is not an attestation. The generated branch is created at the recorded base SHA,
preventing a moving default branch from silently changing the review basis.

## Admission policy

Admission denies a proposal when any of these conditions is true:

- external write capability was not explicitly enabled;
- consent or base revision is missing;
- the permit expired or belongs to another repository;
- file or changed-line budgets are exceeded;
- a path is non-canonical, duplicated, or requests an unsupported deletion;
- a path is outside the allowlist;
- a protected governance, policy, funding, workflow, consent, or agent-instruction path is touched;
- a risk, quality, or validation evidence check failed;
- the operator rejects or skips human review.

## EvidenceCapsule

An admitted proposal receives an evidence capsule with the permit ID, consent source, base SHA,
expiry, change fingerprint, paths, scope totals, checks, and generation time. The fingerprint covers
the complete write-relevant candidate: PR and finding metadata, branch and commit metadata, every
regular change, and every test change. The capsule is included in the draft pull-request description.

The interactive gate renders every proposed file byte and every evidence check without truncating or
omitting test changes. Immediately before the first write, the PR manager recomputes the fingerprint
and scope, rejects expired or failed evidence, then re-reads the current manifest or approved issue.
Revoked or narrowed consent therefore fails closed even if generation and review already completed.

The current capsule is a deterministic local audit receipt. It is not signed by a remote authority
and does not prove that CI passed after submission. Future schema versions may add signatures or
external policy-engine attestations without treating unsigned v2 receipts as stronger than they are.

## Admission audit ledger

ContribAI attempts to record every terminal admission decision in the local SQLite audit
ledger. A record identifies the decision stage and result, candidate fingerprint, repository,
paths, scope totals, check results, reason, and the permit/base SHA when one was issued. Generated
file contents are deliberately excluded.

Each record has a SHA-256 receipt over its fields and the preceding receipt. `contribai admissions`
verifies the complete chain before listing recent entries and exits non-zero if verification fails:

```bash
contribai admissions
contribai admissions --repository owner/repo --decision blocked
contribai admissions --json
```

The web surface exposes `GET /api/admissions` under the server's loopback/API-key access policy; the
default MCP surface exposes the read-only `list_admission_audit` tool. Prometheus reports aggregate
terminal decisions without repository or path labels. An approved candidate cannot proceed if its
audit record fails to persist.

Before appending a decision, the full retained chain is verified in the same SQLite write
transaction. Corrupted history or an unsupported record schema prevents the append, preserving
the stored evidence and blocking approved submissions. See [audit recovery](AUDIT_RECOVERY.md)
for inspection and restoration steps. Payload parsing errors never include the stored values.

The linked receipts detect accidental field mutation, deletion in the middle of a retained chain,
and reordering. They are not signatures, do not establish reviewer identity, and cannot detect an
attacker replacing the entire database plus every independently retained head receipt. Detecting
suffix truncation likewise requires retaining the latest receipt outside the database.

## Non-goals

The protocol does not:

- grant merge rights;
- guarantee code quality or maintainer acceptance;
- authorize legal attestations such as a CLA;
- override repository contribution or security policy;
- authorize vulnerability testing outside an explicit security program;
- make bulk unsolicited GitHub activity acceptable.

## Inspecting consent

`consent-check` reads repository metadata, checks every supported manifest path, and attests the
default branch SHA. It never invokes an LLM and never writes to GitHub:

```bash
contribai consent-check owner/repo
contribai consent-check owner/repo --json --require-consent
```

`--require-consent` exits non-zero unless both a valid manifest and full base revision are available,
which makes the command suitable for CI and policy adapters. A successful check only opens the
repository gate; it does not approve any particular patch.
