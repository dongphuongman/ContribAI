import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const repositoryRoot = dirname(scriptDirectory);
const siteRoot = join(repositoryRoot, "site");
const canonicalUrl = "https://contribai-topaz.vercel.app/";
const deployUrl =
  "https://vercel.com/new/clone?repository-url=https%3A%2F%2Fgithub.com%2Ftang-vu%2FContribAI";

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function readSiteFile(name) {
  const path = join(siteRoot, name);
  assert(existsSync(path), `Missing site/${name}`);
  assert(statSync(path).size > 0 || name === ".nojekyll", `Empty site/${name}`);
  return readFileSync(path, "utf8");
}

function collect(pattern, source) {
  return [...source.matchAll(pattern)].map((match) => match[1]);
}

try {
  const requiredFiles = [
    ".nojekyll",
    "app.js",
    "favicon.svg",
    "index.html",
    "robots.txt",
    "site.webmanifest",
    "sitemap.xml",
    "social-card.svg",
    "styles.css",
  ];

  for (const file of requiredFiles) readSiteFile(file);

  const html = readSiteFile("index.html");
  const readme = readFileSync(join(repositoryRoot, "README.md"), "utf8");
  const vercel = JSON.parse(readFileSync(join(repositoryRoot, "vercel.json"), "utf8"));
  assert(/^<!doctype html>/i.test(html), "index.html must declare an HTML5 doctype");
  assert(/<html\s+lang="en">/i.test(html), "index.html must declare its language");
  assert(/<meta\s+name="viewport"/i.test(html), "index.html must include a viewport");
  assert(/http-equiv="Content-Security-Policy"/i.test(html), "index.html must define a CSP");
  assert(/<main\s+id="main">/i.test(html), "index.html must include the main landmark");
  assert(/<h1\s+id="hero-title">/i.test(html), "index.html must include one primary heading");
  assert(html.includes(`<link rel="canonical" href="${canonicalUrl}">`), "Canonical URL is stale");
  assert(!/<script(?![^>]*\bsrc=)[^>]*>/i.test(html), "Inline scripts are not allowed");
  assert(!/<style(?:\s|>)/i.test(html), "Inline stylesheets are not allowed");
  assert(!/\son[a-z]+\s*=/i.test(html), "Inline event handlers are not allowed");

  assert(vercel.framework === null, "Vercel must use the framework-free static preset");
  assert(vercel.outputDirectory === "site", "Vercel must publish only site/");
  for (const forbiddenCapability of ["builds", "env", "functions", "rewrites"]) {
    assert(!(forbiddenCapability in vercel), `Vercel must not expose ${forbiddenCapability}`);
  }
  const catchAllHeaders = vercel.headers?.find((entry) => entry.source === "/(.*)")?.headers ?? [];
  const responseHeaders = new Map(
    catchAllHeaders.map(({ key, value }) => [key.toLowerCase(), value])
  );
  const requiredResponseHeaders = [
    "content-security-policy",
    "cross-origin-opener-policy",
    "permissions-policy",
    "referrer-policy",
    "x-content-type-options",
    "x-frame-options",
  ];
  for (const header of requiredResponseHeaders) {
    assert(responseHeaders.has(header), `Vercel security header is missing: ${header}`);
  }
  assert(
    responseHeaders.get("content-security-policy").includes("frame-ancestors 'none'"),
    "Vercel CSP must block framing"
  );
  assert(readme.includes(`](${deployUrl})`), "README Vercel Deploy Button is missing or stale");

  const ids = collect(/\bid="([^"]+)"/g, html);
  const duplicateIds = ids.filter((id, index) => ids.indexOf(id) !== index);
  assert(duplicateIds.length === 0, `Duplicate HTML IDs: ${[...new Set(duplicateIds)].join(", ")}`);

  for (const fragment of collect(/href="#([^"]+)"/g, html)) {
    assert(ids.includes(fragment), `Broken fragment link: #${fragment}`);
  }

  for (const target of collect(/data-copy="([^"]+)"/g, html)) {
    assert(ids.includes(target), `Copy button references missing ID: ${target}`);
  }

  for (const asset of collect(/(?:href|src)="(\.\/[^"?#]+)(?:[?#][^"]*)?"/g, html)) {
    const assetPath = join(siteRoot, asset.slice(2));
    assert(existsSync(assetPath), `Broken local asset reference: ${asset}`);
  }

  const scriptSources = collect(/<script[^>]+src="([^"]+)"/g, html);
  assert(scriptSources.every((source) => source.startsWith("./")), "Scripts must be served locally");
  const stylesheets = collect(/<link[^>]+rel="stylesheet"[^>]+href="([^"]+)"/g, html);
  assert(stylesheets.every((source) => source.startsWith("./")), "Stylesheets must be served locally");

  const policyClaims = [
    "public code</strong> ≠ permission to submit",
    "Read-only default",
    "Protected governance paths stay blocked",
    "Every resulting pull request remains a draft",
    "Human approval cannot be delegated",
    "No analytics. No trackers. No cookies.",
  ];
  for (const claim of policyClaims) {
    assert(html.includes(claim), `Missing product-contract language: ${claim}`);
  }

  const commands = ["contribai demo", "contribai init", "contribai consent-check", "contribai analyze"];
  for (const command of commands) assert(html.includes(command), `Missing quick-start command: ${command}`);

  assert(/data-demo-start/.test(html), "Offline walkthrough control is missing");
  assert(
    collect(/data-demo-line(?:\s|>)/g, html).length === 7,
    "Offline walkthrough must render all seven policy steps"
  );

  const manifest = JSON.parse(readSiteFile("site.webmanifest"));
  assert(manifest.name === "ContribAI", "Web manifest name is invalid");
  assert(manifest.start_url === "./" && manifest.scope === "./", "Web manifest must support a project site");
  for (const icon of manifest.icons ?? []) {
    assert(icon.src.startsWith("./"), "Manifest icons must be local");
    assert(existsSync(join(siteRoot, icon.src.slice(2))), `Missing manifest icon: ${icon.src}`);
  }

  const sitemap = readSiteFile("sitemap.xml");
  const robots = readSiteFile("robots.txt");
  assert(sitemap.includes(`<loc>${canonicalUrl}</loc>`), "Sitemap canonical URL is stale");
  assert(robots.includes(`${canonicalUrl}sitemap.xml`), "robots.txt sitemap URL is stale");
  assert(readSiteFile("favicon.svg").includes("<svg"), "Favicon is not SVG");
  assert(readSiteFile("social-card.svg").includes("<svg"), "Social card is not SVG");

  console.log(
    `Static site contract passed (${requiredFiles.length} files, ${ids.length} unique IDs, Vercel static-only).`
  );
} catch (error) {
  console.error(`Static site contract failed: ${error.message}`);
  process.exitCode = 1;
}
