# ContribAI documentation

ContribAI is a maintainer-governed agent for producing evidence-backed contribution proposals.
Start with these documents:

- [Project README](../README.md) — installation, safe defaults, and operator workflow
- [Project website](https://tang-vu.github.io/ContribAI/) — concise public introduction and
  read-only onboarding
- [Five-minute walkthrough](QUICKSTART.md) — offline policy proof before credentials or network
- [Website maintenance](WEBSITE.md) — local preview, contract checks, and deployment
- [Architecture](../ARCHITECTURE.md) — components and trust boundaries
- [Consent protocol](CONSENT_PROTOCOL.md) — repository opt-in and issue-scoped approval
- [Threat model](THREAT_MODEL.md) — assets, adversaries, mitigations, and residual risk
- [Security policy](../SECURITY.md) — supported versions and private reporting
- [Governance](../GOVERNANCE.md) — decision process and project roles
- [Contributing](../CONTRIBUTING.md) — code and AI-assistance requirements

## Documentation status

The files above are normative. Versioned planning documents such as `project-roadmap.md`,
`project-changelog.md`, and `project-overview-pdr.md` are historical records; they may describe
capabilities or product language that no longer represents the safety contract.

The maintained implementation is Rust under `crates/contribai-rs`. Python documentation exists
only for the legacy reference implementation under `python/`.
