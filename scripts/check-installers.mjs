import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = dirname(dirname(fileURLToPath(import.meta.url)));

function read(path) {
  return readFileSync(join(repositoryRoot, path), "utf8");
}

function assert(condition, message) {
  if (!condition) {
    console.error("Installer contract failed: " + message);
    process.exit(1);
  }
}

const cargoToml = read("crates/contribai-rs/Cargo.toml");
const cargoLock = read("Cargo.lock");
const shellInstaller = read("install.sh");
const powershellInstaller = read("install.ps1");
const releaseWorkflow = read(".github/workflows/release.yml");
const version = cargoToml.match(/^version = "([0-9]+\.[0-9]+\.[0-9]+)"$/m)?.[1];

assert(version, "crate version is missing");
assert(
  cargoLock.includes('name = "contribai"\nversion = "' + version + '"'),
  "workspace lockfile version does not match the crate"
);
assert(
  shellInstaller.includes('VERSION="v' + version + '"'),
  "install.sh version does not match the crate"
);
assert(
  powershellInstaller.includes('$Version = "v' + version + '"'),
  "install.ps1 version does not match the crate"
);

for (const asset of [
  "linux-x86_64",
  "macos-aarch64",
  "macos-x86_64",
  "windows-x86_64.exe",
]) {
  assert(releaseWorkflow.includes("asset: " + asset), "release matrix is missing " + asset);
}

assert(
  shellInstaller.includes('CONTRIBAI_INSTALL_DIR:-/usr/local/bin'),
  "install.sh must support isolated installs"
);
assert(
  powershellInstaller.includes("CONTRIBAI_INSTALL_DIR"),
  "install.ps1 must support isolated installs"
);
assert(
  powershellInstaller.includes("CONTRIBAI_NO_PATH_UPDATE"),
  "install.ps1 must support a no-PATH-update smoke mode"
);
assert(
  shellInstaller.includes('CHECKSUM_URL="$URL.sha256"') &&
    powershellInstaller.includes('$ChecksumUrl = "$Url.sha256"'),
  "installers must consume release checksum sidecars"
);
assert(
  releaseWorkflow.includes('contribai-${{ github.ref_name }}-${{ matrix.asset }}.sha256'),
  "release workflow must publish checksum sidecars"
);
assert(
  releaseWorkflow.includes("installer-smoke:") &&
    releaseWorkflow.includes("needs: release"),
  "release workflow must smoke-test installers after every asset is published"
);

console.log("Installer contract passed for ContribAI v" + version + ".");
