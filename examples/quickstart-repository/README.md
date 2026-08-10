# ContribAI quickstart fixture

This tiny JavaScript repository mirrors the scenario exercised by <code>contribai demo</code>. It
exists so a maintainer can inspect a complete opt-in surface without granting access to a real
project.

The repository manifest allows at most two files and 40 changed lines under <code>src/**</code> and
<code>tests/**</code>. It does not authorize changes to the manifest, workflows, governance,
security policy, licensing, or any other protected path.

Run the baseline test from this directory:

    npm test

The offline demo evaluates a focused candidate that normalizes a blank greeting name and adds a
test. It also probes <code>.github/workflows/release.yml</code> and confirms that the protected-path
policy denies it.

This fixture is educational evidence, not maintainer consent for another repository. Only a
maintainer should add or approve a consent manifest in a real target repository.
