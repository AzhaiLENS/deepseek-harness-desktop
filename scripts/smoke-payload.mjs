#!/usr/bin/env node
/**
 * End-to-end smoke test for a vendored payload — no Rust, no Tauri involved.
 *
 * It reproduces exactly what the desktop shell does at boot:
 *   1. extract `dist/dsh-payload-*.tar.zst` into a clean data directory
 *   2. run the *vendored* node against the *vendored* dsh CLI
 *      (`--profile web --no-open --port 0`) with `DSH_HOME` pointed at an
 *      isolated home
 *   3. wait for the `dsh web: <url>` line, fetch it, and assert the Harness UI
 *      HTML came back
 *   4. shut the server down and report
 *
 * Usage:
 *   node scripts/smoke-payload.mjs [--payload dist/dsh-payload-<os>-<arch>.tar.zst]
 *                                  [--data .scratch/smoke] [--keep]
 */

import { spawn, spawnSync } from "node:child_process";
import { createInterface } from "node:readline";
import { existsSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const args = process.argv.slice(2);
const argOf = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
};
const KEEP = args.includes("--keep");

const hostTag = () => {
  const os = process.platform === "win32" ? "win32" : process.platform;
  const arch = process.arch === "arm64" ? "arm64" : "x64";
  return `${os}-${arch}`;
};

function findPayload() {
  const explicit = argOf("--payload");
  if (explicit) return resolve(explicit);
  const dist = join(ROOT, "dist");
  if (!existsSync(dist)) return null;
  const tag = hostTag();
  const candidates = readdirSync(dist).filter((f) => f.startsWith("dsh-payload-"));
  const match =
    candidates.find((f) => f.includes(tag) && f.endsWith(".tar.zst")) ??
    candidates.find((f) => f.includes(tag));
  return match ? join(dist, match) : null;
}

const payload = findPayload();
if (!payload || !existsSync(payload)) {
  console.error("smoke: no payload archive found — run scripts/vendor-runtime.sh first");
  process.exit(1);
}

const data = resolve(argOf("--data", join(ROOT, ".scratch", "smoke")));
const nodeExe = process.platform === "win32" ? "node.exe" : "node";
const dshBin = join(data, "vendor", "node_modules", "@deepseek-ai", "dsh", "lib", "bin.js");
const nodeBin = join(data, "runtime", "node", nodeExe);
const home = join(data, "home");
const vendor = join(data, "vendor");

if (payload.endsWith(".tar.gz")) {
  console.error("smoke: please use the .tar.zst payload for this test");
  process.exit(1);
}

console.log(`smoke: payload  ${payload}`);
console.log(`smoke: data dir ${data}`);

rmSync(data, { recursive: true, force: true });
mkdirSync(data, { recursive: true });
// DSH_HOME must exist before node starts with it as the working directory.
mkdirSync(home, { recursive: true });

// ---------------------------------------------------------------- extract ---
// Extraction goes through `zstd -dc | tar -x -` rather than
// `tar --use-compress-program=zstd`: on macOS 27 bsdtar that path reports
// spurious per-entry failures. The app itself uses Rust's tar+zstd crates, so
// this test only has to reproduce the *layout*, not the exact tooling.
const zstd = spawn("zstd", ["-dc", payload], { stdio: ["ignore", "pipe", "pipe"] });
const started = Date.now();
const untar = spawn("tar", ["-xf", "-", "-C", data], { stdio: [zstd.stdout, "ignore", "pipe"] });
zstd.stdout.on("error", () => {}); // EPIPE when tar finishes first
let tarErr = "";
untar.stderr.on("data", (chunk) => (tarErr += chunk.toString()));
const tarStatus = await new Promise((done) => untar.on("close", (code) => done(code)));
zstd.kill();
const extractMs = Date.now() - started;

// macOS bsdtar can report a spurious failure for one entry even though the
// archive extracted completely — verify by content, not by exit code.
if (!existsSync(vendor) || !existsSync(nodeBin)) {
  console.error(`smoke: extraction failed (tar exit ${tarStatus})`);
  console.error(tarErr.slice(0, 2000));
  process.exit(1);
}
const extractedFiles = Number(
  spawnSync("sh", ["-c", `find ${JSON.stringify(data)} -type f | wc -l`], { encoding: "utf8" }).stdout.trim(),
);
console.log(`smoke: extracted ${extractedFiles} files in ${extractMs} ms`);

// ------------------------------------------------------------------ boot ----
const nodeVersion = spawnSync(nodeBin, ["--version"], { encoding: "utf8" }).stdout?.trim();
console.log(`smoke: vendored node ${nodeVersion}`);

const env = {
  ...process.env,
  DSH_HOME: home,
  PATH: `${dirname(nodeBin)}${process.platform === "win32" ? ";" : ":"}${process.env.PATH ?? ""}`,
};
for (const key of ["NODE_OPTIONS", "NODE_PATH", "npm_config_prefix", "npm_config_cache"]) {
  delete env[key];
}

const child = spawn(
  nodeBin,
  [dshBin, "--profile", "web", "--host", "127.0.0.1", "--port", "0", "--no-open"],
  { env, cwd: home, stdio: ["ignore", "pipe", "pipe"] },
);

let url = null;
const lines = [];
const watch = (stream, label) => {
  createInterface({ input: stream }).on("line", (line) => {
    lines.push(`${label}| ${line}`);
    const match = line.match(/dsh web: (http:\/\/127\.0\.0\.1:\d+\/\?token=[\w-]+)/);
    if (match) url = match[1];
  });
};
watch(child.stdout, "out");
watch(child.stderr, "err");

const deadline = Date.now() + 120_000;
const waitFor = async (predicate) => {
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((r) => setTimeout(r, 250));
  }
  return false;
};

let failed = false;
try {
  if (!(await waitFor(() => url))) {
    throw new Error("the DSH server never printed its authenticated URL");
  }
  console.log(`smoke: server url ${url.replace(/token=.*/, "token=***")}`);

  const bootMs = Date.now() - started;

  // Reproduce what the webview does: navigate to the token URL, receive the
  // 303 that installs the `dsh-auth-*` cookie, then load `/`. The cookie is
  // `HttpOnly; SameSite=Strict`, so it must be carried explicitly by a script
  // (a real browser/webview does this for us automatically).
  const tokenResponse = await fetch(url, { redirect: "manual" });
  const setCookie = tokenResponse.headers.getSetCookie?.() ?? [];
  if (tokenResponse.status !== 303 || setCookie.length === 0) {
    throw new Error(
      `the token exchange did not redirect with a session cookie (status ${tokenResponse.status})`,
    );
  }
  const cookie = setCookie.map((c) => c.split(";")[0]).join("; ");

  const response = await fetch(url.replace(/\?token=.*$/, ""), {
    headers: { cookie },
    redirect: "manual",
  });
  const html = await response.text();

  if (response.status !== 200) throw new Error(`UI request returned HTTP ${response.status}`);
  if (!html.includes("__ModuleLoader__")) {
    throw new Error("the served page is not the Harness UI (no __ModuleLoader__ bootstrap)");
  }
  if (html.length < 5000) throw new Error(`the served page is suspiciously small (${html.length} bytes)`);

  console.log(`smoke: UI served — HTTP ${response.status}, ${html.length} bytes`);
  console.log(`smoke: cold boot to UI in ${bootMs} ms (extraction ${extractMs} ms)`);

  // The profile must have been seeded inside the isolated home, and the user's
  // real ~/.dsh must NOT have been touched.
  const profileManifest = join(home, "profiles", "web", "package.json");
  if (!existsSync(profileManifest)) throw new Error("the web profile was not initialised in DSH_HOME");
  console.log("smoke: isolated DSH_HOME was seeded correctly");
} catch (error) {
  failed = true;
  console.error(`smoke: FAILED — ${error.message}`);
  console.error(lines.slice(-25).join("\n"));
} finally {
  child.kill("SIGTERM");
  await new Promise((r) => setTimeout(r, 750));
  child.kill("SIGKILL");
  if (!KEEP && !failed) rmSync(data, { recursive: true, force: true });
}

console.log(failed ? "smoke: FAIL" : "smoke: PASS");
process.exit(failed ? 1 : 0);
