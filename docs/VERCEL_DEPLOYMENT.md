# Vercel deployment

ContribAI's production website at <https://contribai-topaz.vercel.app/> hosts the public,
dependency-free onboarding site in `site/`. It does not host the authenticated Rust dashboard or
any contribution workflow. The deployed website therefore receives no GitHub token, LLM key, or
database, and contains no serverless function, form, application analytics, or external write
capability.

## Import the maintained repository

The repository owner should import the existing `tang-vu/ContribAI` repository from the Vercel
dashboard rather than use the README's clone-oriented Deploy Button:

1. Open [Add New Project](https://vercel.com/new) and import `tang-vu/ContribAI`.
2. Select the Vercel account or team that will own the open source project.
3. Keep the repository root as the Vercel Root Directory.
4. Confirm the Framework Preset is **Other**. `vercel.json` also pins `framework` to `null`.
5. Do not add environment variables. `vercel.json` publishes `site/` as the Output Directory.
6. Deploy, then keep the Git integration enabled so `main` produces production deployments and
   pull requests receive isolated previews.

The README's Deploy Button remains intentional: it lets other maintainers clone and inspect the
static site on their own Vercel account, and satisfies the Vercel OSS Program's current request for
a README deployment link.

## Verify the production deployment

Replace the example URL and check both content and response headers:

```bash
export CONTRIBAI_SITE_URL="https://contribai-topaz.vercel.app"
curl --fail --silent --show-error "$CONTRIBAI_SITE_URL/" > /dev/null
curl --head "$CONTRIBAI_SITE_URL/"
```

The response should include the Content Security Policy, `Referrer-Policy: no-referrer`,
`X-Content-Type-Options: nosniff`, and `X-Frame-Options: DENY`. The page's offline walkthrough
must still work without network requests.

The stable production alias is the canonical website. If that domain changes, update every
location named in [Website maintenance](WEBSITE.md), run the site checks, and update the GitHub
repository homepage. Do not point canonical metadata at a preview deployment or a commit-specific
URL.

## Vercel OSS Program handoff

The application form requires the live project URL and the owning Vercel Team ID. After the first
production deployment:

1. copy the stable production URL from the Vercel project overview;
2. copy the Team ID from the team's [General settings](https://vercel.com/d?to=/%5Bteam%5D/~/settings&title=Find+Team+ID);
3. complete the evidence-backed [application worksheet](VERCEL_OSS_APPLICATION.md);
4. submit the form only after personally reviewing its Code of Conduct, program terms, and privacy
   policy.

Never commit the Team ID, personal contact fields, Vercel access tokens, or generated `.vercel/`
linkage directory.
