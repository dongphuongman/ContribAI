import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const releaseBase = "https://github.com/tang-vu/ContribAI/releases/";
const bytes = Buffer.from("verified fixture binary\n");
const hash = createHash("sha256").update(bytes).digest("hex");
const oldBytes = Buffer.from("existing installation\n");
const gitExec = spawnSync("git", ["--exec-path"], { encoding: "utf8" }).stdout?.trim();
const bash = process.platform === "win32"
  ? resolve(gitExec, "../../..", "bin/bash.exe") : "bash";
const engines = [{ name: "bash", executable: bash, script: "install.sh" }];
if (process.platform === "win32") {
  engines.push({ name: "powershell", executable: "powershell.exe", script: "install.ps1" });
  const core = spawnSync("pwsh.exe", ["-NoProfile", "-Command", "exit 0"]);
  if (!core.error && core.status === 0) {
    engines.push({ name: "pwsh", executable: "pwsh.exe", script: "install.ps1" });
  } else if (process.env.CI) {
    throw new Error("Windows CI must exercise both PowerShell 5.1 and PowerShell 7");
  }
}

const bashHarness = `
set -euo pipefail
uname() {
  case "$1" in -s) printf '%s' "$MOCK_OS" ;; -m) printf '%s' "$MOCK_ARCH" ;; esac
}
curl() {
  local output='' url=''
  while [ "$#" -gt 0 ]; do
    case "$1" in -o) shift; output="$1" ;; https://*) url="$1" ;; esac
    shift
  done
  printf '%s\\n' "$url" >> "$MOCK_LOG"
  if [ "$url" = 'https://github.com/tang-vu/ContribAI/releases/latest' ]; then
    [ "$MOCK_FAIL" != resolve ] || return 22
    printf '%s' "$MOCK_LATEST"
  elif [ "$url" = "$MOCK_BINARY_URL" ]; then
    [ "$MOCK_FAIL" != binary ] || return 22
    printf '%s' "$MOCK_BINARY" > "$output"
  elif [ "$url" = "$MOCK_BINARY_URL.sha256" ]; then
    [ "$MOCK_FAIL" != checksum ] || return 22
    printf '%s' "$MOCK_CHECKSUM" > "$output"
  else
    return 22
  fi
}
source "$MOCK_INSTALLER"
`;

const powershellHarness = `
$ErrorActionPreference = 'Stop'
function Invoke-WebRequest {
  param([string]$Uri, [string]$OutFile, [string]$Method, [switch]$UseBasicParsing,
        [int]$MaximumRedirection, [int]$TimeoutSec)
  Add-Content -LiteralPath $env:MOCK_LOG -Value $Uri -Encoding utf8
  if ($Uri -eq 'https://github.com/tang-vu/ContribAI/releases/latest') {
    if ($env:MOCK_FAIL -eq 'resolve') { throw 'PRIVATE_RESPONSE_BODY' }
    $location = [Uri]$env:MOCK_LATEST
    return [pscustomobject]@{ BaseResponse = [pscustomobject]@{
      ResponseUri = $location
      RequestMessage = [pscustomobject]@{ RequestUri = $location }
    }}
  } elseif ($Uri -eq $env:MOCK_BINARY_URL) {
    if ($env:MOCK_FAIL -eq 'binary') { throw 'PRIVATE_RESPONSE_BODY' }
    [IO.File]::WriteAllBytes($OutFile, [Convert]::FromBase64String($env:MOCK_BINARY_BASE64))
  } elseif ($Uri -eq "$env:MOCK_BINARY_URL.sha256") {
    if ($env:MOCK_FAIL -eq 'checksum') { throw 'PRIVATE_RESPONSE_BODY' }
    [IO.File]::WriteAllText($OutFile, $env:MOCK_CHECKSUM, [Text.Encoding]::ASCII)
  } else {
    throw 'Unexpected network request'
  }
}
& $env:MOCK_INSTALLER
`;

function runInstaller(t, engine, options = {}) {
  const directory = mkdtempSync(join(tmpdir(), "contribai-installer-"));
  t.after(() => {
    assert.equal(dirname(resolve(directory)), resolve(tmpdir()));
    rmSync(directory, { recursive: true, force: true });
  });
  const bin = join(directory, options.directory || "bin");
  const temporary = join(directory, "tmp");
  mkdirSync(bin);
  mkdirSync(temporary);
  const installed = join(bin, engine.name === "bash" ? "contribai" : "contribai.exe");
  writeFileSync(installed, oldBytes);
  const installer = join(directory, engine.script);
  writeFileSync(installer, readFileSync(join(root, engine.script), "utf8").replace(/\r\n/g, "\n"));
  const harness = join(directory, engine.name === "bash" ? "harness.sh" : "harness.ps1");
  writeFileSync(harness, engine.name === "bash" ? bashHarness : powershellHarness);
  const version = options.pin || "v7.2.3";
  const platform = engine.name === "bash" ? (options.platform || "linux-x86_64") : "windows-x86_64.exe";
  const name = `contribai-${version}-${platform}`;
  const log = join(directory, "requests.log");
  const env = {
    ...process.env,
    CONTRIBAI_VERSION: options.pin || "",
    CONTRIBAI_INSTALL_DIR: bin.replaceAll("\\", "/"),
    CONTRIBAI_NO_PATH_UPDATE: "1",
    TMPDIR: temporary.replaceAll("\\", "/"), TEMP: temporary, TMP: temporary,
    MOCK_OS: platform.startsWith("macos") ? "Darwin" : "Linux",
    MOCK_ARCH: platform.endsWith("aarch64") ? "arm64" : "x86_64",
    MOCK_BINARY: bytes.toString(), MOCK_BINARY_BASE64: bytes.toString("base64"),
    MOCK_BINARY_URL: `${releaseBase}download/${version}/${name}`,
    MOCK_CHECKSUM: options.checksum ?? `${hash}  ${name}\n`,
    MOCK_LATEST: options.latest || `${releaseBase}tag/v7.2.3`,
    MOCK_FAIL: options.fail || "", MOCK_LOG: log.replaceAll("\\", "/"),
    MOCK_INSTALLER: installer.replaceAll("\\", "/"),
  };
  const args = engine.name === "bash" ? [harness.replaceAll("\\", "/")]
    : ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", harness];
  const result = spawnSync(engine.executable, args, { env, encoding: "utf8", timeout: 60000 });
  assert.ifError(result.error);
  if (options.reject) {
    assert.notEqual(result.status, 0, result.stdout);
    assert.deepEqual(readFileSync(installed), oldBytes, "failure must preserve the installed binary");
  } else {
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.deepEqual(readFileSync(installed), bytes);
  }
  assert.deepEqual(readdirSync(temporary), [], "temporary downloads must be cleaned up");
  assert(!`${result.stdout}${result.stderr}`.includes("PRIVATE_RESPONSE_BODY"));
  const requests = existsSync(log) ? readFileSync(log, "utf8").replace(/^\uFEFF/, "").trim().split(/\r?\n/) : [];
  if (options.pin) assert(!requests.some((url) => url.endsWith("/latest")));
  if (!options.reject) assert(requests.includes(env.MOCK_BINARY_URL));
  if (options.noDownloads) assert(!requests.some((url) => url.includes("/download/")));
}

for (const engine of engines) {
  test(`${engine.name}: install into a literal path with spaces and brackets`, (t) => runInstaller(t, engine, { directory: "bin [verified]" }));
  test(`${engine.name}: install the published release independent of source version`, (t) => runInstaller(t, engine));
  test(`${engine.name}: explicit version bypasses latest lookup`, (t) => runInstaller(t, engine, { pin: "v6.10.0" }));
  for (const pin of ["../v6.10.0", "v6.10.0\nextra", "v6.10.0\n", "v01.2.3", "https://other.example/file"]) {
    test(`${engine.name}: reject invalid pin ${JSON.stringify(pin)}`, (t) => runInstaller(t, engine, { pin, reject: true, noDownloads: true }));
  }
  for (const latest of ["https://other.example/releases/tag/v7.2.3", `${releaseBase}tag/v7.2.3-rc.1`, `${releaseBase}tag/v7.2.3?next=bad`]) {
    test(`${engine.name}: reject unexpected latest destination ${latest}`, (t) => runInstaller(t, engine, { latest, reject: true, noDownloads: true }));
  }
  for (const fail of ["resolve", "binary", "checksum"]) {
    test(`${engine.name}: ${fail} failure preserves installation`, (t) => runInstaller(t, engine, { fail, reject: true }));
  }
  for (const checksum of ["not-a-checksum", `${"0".repeat(64)}  fixture\n`]) {
    test(`${engine.name}: reject malformed or mismatched checksum ${checksum.slice(0, 12)}`, (t) => runInstaller(t, engine, { checksum, reject: true }));
  }
}
for (const platform of ["macos-x86_64", "macos-aarch64"]) {
  test(`bash: select ${platform}`, (t) => runInstaller(t, engines[0], { platform }));
}
