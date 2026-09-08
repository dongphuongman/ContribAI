# ContribAI documentation

ContribAI is a maintainer-governed agent for producing evidence-backed contribution proposals.
Start with these documents:

- [Project README](../README.md) — installation, safe defaults, and operator workflow
- [Project website](https://contribai-topaz.vercel.app/) — concise public introduction and
  read-only onboarding
- [Five-minute walkthrough](QUICKSTART.md) — offline policy proof before credentials or network
- [Deployment guide](deployment-guide.md) ? source builds, dashboard, Docker, and recovery
- [Website maintenance](WEBSITE.md) — local preview, contract checks, and deployment
- [Vercel deployment](VERCEL_DEPLOYMENT.md) — static hosting boundary and production setup
- [Vercel OSS application](VERCEL_OSS_APPLICATION.md) — evidence-backed application worksheet
- [Architecture](../ARCHITECTURE.md) — components and trust boundaries
- [Consent protocol](CONSENT_PROTOCOL.md) — repository opt-in and issue-scoped approval
- [Audit recovery](AUDIT_RECOVERY.md) — integrity failures, evidence preservation, and restoration
- [Threat model](THREAT_MODEL.md) — assets, adversaries, mitigations, and residual risk
- [Security policy](../SECURITY.md) — supported versions and private reporting
- [Governance](../GOVERNANCE.md) — decision process and project roles
- [Contributing](../CONTRIBUTING.md) — code and AI-assistance requirements
- [Releasing](RELEASING.md) — candidate verification, publication gates, and failure handling
- [Current roadmap](project-roadmap.md) — delivery phases and their completion evidence

## Documentation status

The files above are normative. The explicitly archived portion of `project-roadmap.md`, along with
`project-changelog.md` and `project-overview-pdr.md`, contains historical records; these may describe
capabilities or product language that no longer represents the safety contract.

The maintained implementation is Rust under `crates/contribai-rs`. Python documentation exists
only for the legacy reference implementation under `python/`.
