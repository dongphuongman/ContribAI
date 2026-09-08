import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat, readFile, readdir, realpath } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export const platforms = [
  "linux-x86_64", "windows-x86_64.exe", "macos-x86_64", "macos-aarch64",
];

/** Validate the complete staged release before granting publication. */
export async function verifyReleaseAssets(directory, tag) {
  if (!/^v\d+\.\d+\.\d+$/.test(tag)) throw new Error("Invalid release tag");
  const binaries = platforms.map((platform) => `contribai-${tag}-${platform}`);
  const expected = binaries.flatMap((name) => [name, `${name}.sha256`]).sort();
  const actual = (await readdir(directory)).sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error("Release must contain exactly four binaries and four checksum sidecars");
  }
  for (const name of expected) {
    const stat = await lstat(join(directory, name));
    if (!stat.isFile() || stat.size === 0) throw new Error(`Invalid release file: ${name}`);
  }
  for (const name of binaries) {
    const sidecar = await readFile(join(directory, `${name}.sha256`), "utf8");
    const match = /^([a-fA-F0-9]{64}) [ *]([^\r\n]+)\r?\n?$/.exec(sidecar);
    if (!match || match[2] !== name) throw new Error(`Invalid checksum sidecar: ${name}`);
    const hash = createHash("sha256");
    for await (const chunk of createReadStream(join(directory, name))) hash.update(chunk);
    if (hash.digest("hex") !== match[1].toLowerCase()) {
      throw new Error(`Checksum mismatch: ${name}`);
    }
  }
}

if (process.argv[1] && await realpath(process.argv[1]) === await realpath(fileURLToPath(import.meta.url))) {
  try {
    await verifyReleaseAssets(process.argv[2], process.argv[3]);
    console.log("Complete release asset set and checksums verified.");
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
