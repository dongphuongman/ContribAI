# Project website

The public website at <https://tang-vu.github.io/ContribAI/> is an onboarding surface for
maintainers and contributors. It explains the admission model and points people toward read-only
commands before they consider granting submission capability. It is not the authenticated local
dashboard under `crates/contribai-rs/src/web/` and cannot operate a ContribAI runtime.

## Design constraints

- Everything is served from `site/`; there is no package manager or build step.
- Scripts, styles, icons, and social artwork are repository-local.
- The page has no analytics, trackers, cookies, forms, backend calls, or external fonts.
- The walkthrough animation is user-triggered, honors reduced-motion preferences, and mirrors the
  machine-readable output of `contribai demo`.
- The content security policy disables network connections and form submission.
- Claims about submission, consent, evidence, and human review must match the product contract in
  `AGENTS.md`.
- Relative asset paths keep the site valid under the `/ContribAI/` GitHub Pages project path.

## Preview and validate

From the workspace root, start any static HTTP server. For example:

```bash
python -m http.server 8000 --directory site
```

Then open <http://localhost:8000>. Before committing, run:

```bash
node scripts/check-site.mjs
node --check site/app.js
```

The contract check catches missing assets, broken fragment links, duplicate IDs, remote executable
assets, inline scripts or event handlers, stale canonical URLs, incomplete demo steps, and removal
of core safety claims.

## Deployment

`.github/workflows/pages.yml` validates pull requests and deploys `site/` from `main`. All actions
are pinned to full commit SHAs. The validation job has read-only repository access; only the deploy
job receives `pages: write` and OIDC token permissions.

The repository's Pages source must be set to **GitHub Actions**. A new deployment runs when the
site, its contract check, or the Pages workflow changes. If the public URL changes, update the
canonical and Open Graph URLs in `site/index.html`, plus `site/robots.txt`, `site/sitemap.xml`, and
`scripts/check-site.mjs` in the same commit.
