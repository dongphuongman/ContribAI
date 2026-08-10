# Governance

ContribAI is maintained in public with a bias toward maintainer autonomy, verifiable behavior, and
low ecosystem externalities.

## Roles

- **Contributors** propose code, documentation, tests, designs, or reviews.
- **Maintainers** triage issues, review and merge changes, manage releases, and enforce project
  policy.
- **Security maintainers** coordinate private vulnerability reports and embargoed fixes.

Current ownership is recorded in [.github/CODEOWNERS](.github/CODEOWNERS) and
[MAINTAINERS.md](MAINTAINERS.md). Ownership should expand based on sustained, trustworthy work—not
commit count or generated output volume.

## Decision process

Routine changes are decided through pull-request review. Significant decisions require a public
design issue and a recorded rationale. Significant decisions include:

- weakening or changing an admission invariant;
- changing license or governance;
- introducing a new external write surface;
- changing credential scope, release signing, telemetry, or data retention;
- making a breaking configuration or protocol change.

Maintainers seek rough consensus. If consensus is not possible, the project lead makes the decision
and records the trade-off publicly. Security embargoes are the exception to public deliberation.

## Guardrail changes

Safety defaults may be strengthened in a minor release. Weakening a default requires:

1. a documented maintainer/user need;
2. a threat-model update;
3. deny-path regression tests;
4. an explicit migration note;
5. review from a maintainer who did not author the change, whenever practical.

No compatibility promise requires preserving behavior that can create unsolicited activity,
misrepresent a person, sign a legal attestation, or expose credentials.

## Releases

- Releases are built from tagged commits by GitHub Actions.
- Release workflows use least-privilege permissions and immutable action references.
- Published binaries include checksums and provenance attestations.
- The changelog describes security, behavior, compatibility, and migration impact.
- A release may be yanked or superseded if its integrity or safety properties are uncertain.

## Conflict of interest

Reviewers should disclose material conflicts, including commercial relationships with affected
providers or repositories. They should recuse themselves when impartial review is not credible.

## Changes to governance

Governance changes follow the significant-decision process. Emergency security changes may land
privately first but must be documented after coordinated disclosure.
