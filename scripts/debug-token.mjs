#!/usr/bin/env node
// Diagnose the DSH web token exchange against a live server (dev helper).
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { mkdirSync } from "node:fs";
import { join, resolve } from "node:path";

const data = resolve(".scratch/smoke");
const home = join(data, "home");
mkdirSync(home, { recursive: true });
const nodeBin = join(data, "runtime/node/node");
const dshBin = join(data, "vendor/node_modules/@deepseek-ai/dsh/lib/bin.js");

const child = spawn(
  nodeBin,
  [dshBin, "--profile", "web", "--host", "127.0.0.1", "--port", "55811", "--no-open"],
  { cwd: home, env: { ...process.env, DSH_HOME: home }, stdio: ["ignore", "pipe", "pipe"] },
);
let url = null;
createInterface({ input: child.stdout }).on("line", (l) => {
  const m = l.match(/dsh web: (http:\/\/127\.0\.0\.1:\d+\/\?token=[\w-]+)/);
  if (m) url = m[1];
});
createInterface({ input: child.stderr }).on("line", (l) => process.stderr.write(`[srv] ${l}\n`));

const wait = async (t) => new Promise((r) => setTimeout(r, t));
for (let i = 0; i < 80 && !url; i++) await wait(250);
if (!url) {
  console.error("no url");
  child.kill();
  process.exit(1);
}
console.log("url:", url.replace(/token=.*/, "token=***"));

const r1 = await fetch(url, { redirect: "manual" });
console.log("A) manual  status:", r1.status, "location:", r1.headers.get("location"));
console.log("A) cookies:", JSON.stringify(r1.headers.getSetCookie?.() ?? r1.headers.get("set-cookie")));

const token = new URL(url).searchParams.get("token");
const bare = url.replace(`?token=${token}`, "");
const r2 = await fetch(bare, {
  redirect: "manual",
  headers: { cookie: (r1.headers.getSetCookie?.() ?? []).map((c) => c.split(";")[0]).join("; ") },
});
console.log("B) bare+cookie status:", r2.status, "len:", (await r2.text()).length);

const r3 = await fetch(url, { redirect: "manual" });
console.log("C) token reuse status:", r3.status);

child.kill("SIGTERM");
await wait(500);
child.kill("SIGKILL");
