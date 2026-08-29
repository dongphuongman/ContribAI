# Vercel OSS Program application worksheet

This worksheet was prepared from the public application and repository evidence on 2026-08-30.
Recheck the [official program page](https://vercel.com/open-source-program) and the live form before
submitting. Keep personal contact details and the Vercel Team ID out of the repository.

## Eligibility evidence

| Program criterion | ContribAI evidence |
|---|---|
| Open source and maintained | AGPL-3.0-or-later, 274 commits, 73 releases, and a v6.9.0 release published on 2026-08-10 |
| Hosted or intended for Vercel | Production site on `contribai-topaz.vercel.app`, root `vercel.json`, a static-only deployment boundary, and a README Deploy Button |
| Impact or growth potential | 244 GitHub stars, 86 forks, 7 contributors, and 379 release-asset downloads |
| Community standards | Repository [Code of Conduct](../CODE_OF_CONDUCT.md), governance, contributing, and security policies |
| Credits restricted to OSS | Documented plan to use Vercel only for ContribAI's public site, previews, and future read-only educational surfaces |

The counts above are a dated snapshot. Immediately before submission, refresh public repository
counts and retain a private screenshot of Vercel deployment status and GitHub Traffic. Owner
analytics for 2026-08-14 through 2026-08-27 recorded 94 clones from 52 unique cloners and 189 views
from 29 unique visitors.

## Form fields

Use the following factual answers as a draft. Personal fields must be reviewed and entered by the
applicant.

- First and last name: use the applicant's legal/preferred name as it appears on the Vercel account.
- Email: use the email attached to that Vercel account.
- Company: leave blank unless ContribAI is actually operated by a company.
- Preferred social link: `https://github.com/tang-vu` is a verifiable project-owner profile.
- Project name: `ContribAI`.
- Live URL: `https://contribai-topaz.vercel.app/`.
- GitHub repository: `https://github.com/tang-vu/ContribAI`.
- Role: `Project owner`, if the applicant remains the repository owner at submission time.

### Tell us about your project

> ContribAI is an AGPL-licensed Rust tool that helps maintainers and accountable contributors
> prepare AI-assisted code changes without treating public repositories as permission to submit.
> It combines repository opt-in, deterministic admission checks, exact human review, and
> evidence-backed draft pull requests.

### What distinguishes your project from other open source projects?

> Most contribution agents optimize for output volume; ContribAI is designed around maintainer
> consent and review cost. External writes are off by default. Submission requires an operator
> capability plus a maintainer-controlled manifest or issue label, a time-limited permit bound to
> the exact repository and base SHA, protected-path and scope gates, passing checks, review of the
> exact candidate, and a draft-only pull request with an evidence receipt. Failed or missing checks
> fail closed, MCP remains read-only by default, and the project never merges or signs legal
> attestations. Its public site provides a privacy-preserving offline walkthrough so developers can
> inspect those boundaries before providing credentials.

### Anything else we should know?

> As of August 30, 2026, ContribAI has 244 GitHub stars, 86 forks, 7 contributors, 274 commits, and
> 379 release-asset downloads; the latest two-week owner analytics recorded 94 clones from 52
> unique cloners. The project ships 686 passing tests and publishes checksummed release binaries.
> Its Vercel surface is intentionally static and read-only: no tokens, forms, analytics, cookies,
> or backend calls. Vercel will provide the public documentation and onboarding site plus Git-linked
> previews. Credits will be used only for ContribAI's open source site and future non-mutating
> educational experiences; the credential-bearing Rust runtime remains user-operated. The project
> also maintains a Code of Conduct, governance policy, security policy, threat model, and explicit
> AI-assistance accountability requirements.

## Attestations that require the applicant

Do not treat this worksheet as consent on the applicant's behalf.

- Confirm the project will remain fully open source only if that is the maintainer's actual intent.
- Confirm Vercel hosting only after a production deployment exists and the maintainer intends to
  keep it there for the full program term.
- Copy the Vercel Team ID from the owning team's settings; do not use a project ID.
- Read Vercel's linked Code of Conduct before agreeing.
- Read the complete program terms and privacy policy shown at submission time before agreeing.

## Current term discrepancy

On 2026-08-30, the public program page advertised $3,600 in credits over three years and described
graduation after three years. The embedded application agreement still described a twelve-month
membership, reapplication after expiry, required Vercel attribution if accepted, and a README
deployment link. The repository now has the deployment link, but it must not claim Vercel support
or add the required acceptance attribution before acceptance.

If the duration changes the decision to apply, ask Vercel to clarify the governing term before
selecting **Yes**. Do not assume the marketing page silently overrides the agreement presented in
the form.
