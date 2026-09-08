import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, writeFile, readFile, rm, symlink } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { platforms, verifyReleaseAssets } from "./check-release-assets.mjs";

const tag = "v6.10.0";
const first = `contribai-${tag}-${platforms[0]}`;
async function fixture(t, newline = "\n") {
  const directory = await mkdtemp(join(tmpdir(), "contribai-release-test-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  for (const platform of platforms) {
    const name = `contribai-${tag}-${platform}`;
    const hash = createHash("sha256").update(name).digest("hex");
    await writeFile(join(directory, name), name);
    await writeFile(join(directory, `${name}.sha256`), `${hash}  ${name}${newline}`);
  }
  return directory;
}

for (const newline of ["\n", "\r\n"]) {
  test(`accept complete release with ${JSON.stringify(newline)} sidecars`, async (t) => {
    await verifyReleaseAssets(await fixture(t, newline), tag);
  });
}
test("reject modified binary", async (t) => {
  const directory = await fixture(t);
  await writeFile(join(directory, first), "modified");
  await assert.rejects(verifyReleaseAssets(directory, tag), /Checksum mismatch/);
});
test("reject missing platform", async (t) => {
  const directory = await fixture(t);
  await rm(join(directory, first));
  await assert.rejects(verifyReleaseAssets(directory, tag), /exactly four/);
});
test("reject unexpected asset", async (t) => {
  const directory = await fixture(t);
  await writeFile(join(directory, "unexpected"), "extra");
  await assert.rejects(verifyReleaseAssets(directory, tag), /exactly four/);
});
test("reject checksum for another filename", async (t) => {
  const directory = await fixture(t);
  const path = join(directory, `${first}.sha256`);
  await writeFile(path, (await readFile(path, "utf8")).replace(first, "../other"));
  await assert.rejects(verifyReleaseAssets(directory, tag), /Invalid checksum/);
});
test("reject empty binary", async (t) => {
  const directory = await fixture(t);
  await writeFile(join(directory, first), "");
  await assert.rejects(verifyReleaseAssets(directory, tag), /Invalid release file/);
});
test("reject malformed tag", async (t) => {
  await assert.rejects(verifyReleaseAssets(await fixture(t), "../release"), /Invalid release tag/);
});

test("CLI verifies real release files and fails on corruption", async (t) => {
  const directory = await fixture(t);
  const script = fileURLToPath(new URL("./check-release-assets.mjs", import.meta.url));
  const run = () => spawnSync(process.execPath, [script, directory, tag], { encoding: "utf8" });
  const valid = run();
  assert.equal(valid.status, 0, valid.stderr);
  assert.match(valid.stdout, /checksums verified/);
  await writeFile(join(directory, first), "corrupt");
  const invalid = run();
  assert.equal(invalid.status, 1);
  assert.match(invalid.stderr, /Checksum mismatch/);
});

test("CLI runs through a directory symlink or Windows junction", async (t) => {
  const directory = await fixture(t);
  const scripts = join(directory, "scripts-link");
  await symlink(dirname(fileURLToPath(import.meta.url)), scripts, "junction");
  // The extra directory also proves verification actually executes and fails closed.
  const result = spawnSync(process.execPath,
    [join(scripts, "check-release-assets.mjs"), directory, tag], { encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /exactly four/);
});
