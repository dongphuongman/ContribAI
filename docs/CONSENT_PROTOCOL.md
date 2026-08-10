# ContribAI Consent Protocol

Status: experimental, schema version 1.

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
| `enabled` | yes | Must be exactly `true`; absence or `false` denies submission |
| `max_files` | no | Maximum changed and test files; default `5` |
| `max_changed_lines` | no | Approximate added-plus-removed line budget; default `250` |
| `allowed_paths` | no | YAML list or comma-separated glob allowlist; empty means any non-protected code path |

Unknown fields are ignored for forward compatibility. Invalid positive integers or invalid path
patterns fail closed for affected proposals. Consent files never authorize changes to protected
governance or automation paths.

Removing the manifest or changing `enabled` to `false` revokes repository consent for future
permits. Existing draft pull requests remain visible and can be closed normally.

## Issue-scoped consent

A maintainer can approve a single issue by applying one of:

- `contribai-approved` (canonical)
- `agent-ready` (compatibility alias)
- `ai-contribution-approved` (compatibility alias)

GitHub normally restricts label application to users with repository triage permission or above.
ContribAI treats the label as intent to receive a proposal for that issue, not as acceptance of the
result. Removing the label prevents future permits.

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

Permits expire after 24 hours. The generated branch is created at the recorded base SHA, preventing
a moving default branch from silently changing the review basis.

## Admission policy

Admission denies a proposal when any of these conditions is true:

- external write capability was not explicitly enabled;
- consent or base revision is missing;
- the permit expired or belongs to another repository;
- file or changed-line budgets are exceeded;
- a path is outside the allowlist;
- a protected governance, policy, funding, workflow, consent, or agent-instruction path is touched;
- a risk, quality, or validation evidence check failed;
- the operator rejects or skips human review.

## EvidenceCapsule

An admitted proposal receives an evidence capsule with the permit ID, consent source, base SHA,
change fingerprint, paths, scope totals, checks, and generation time. The capsule is included in the
draft pull-request description.

The current capsule is a deterministic local audit receipt. It is not signed by a remote authority
and does not prove that CI passed after submission. Future schema versions may add signatures or
external policy-engine attestations without treating unsigned v1 receipts as stronger than they are.

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

`--require-consent` exits non-zero unless both a valid manifest and base revision are available,
which makes the command suitable for CI and policy adapters. A successful check only opens the
repository gate; it does not approve any particular patch.
