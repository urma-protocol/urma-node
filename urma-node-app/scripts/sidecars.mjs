// Builds urma-node and urma-node-control for the bundle target and copies them
// to src-tauri/binaries/<name>-<triple>[.exe], where Tauri's externalBin looks.
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const app = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const root = resolve(app, "..");
const host = execFileSync("rustc", ["-vV"], { encoding: "utf8" })
  .split("\n")
  .find((line) => line.startsWith("host: "))
  .slice("host: ".length);
const triple = process.env.TAURI_ENV_TARGET_TRIPLE || host;
const debug = process.env.TAURI_ENV_DEBUG === "true";
const targetDir = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const exe = triple.includes("windows") ? ".exe" : "";

const args = ["build", "--locked", "-p", "urma-node", "--bins"];
if (!debug) args.push("--release");
if (triple !== host) args.push("--target", triple);
execFileSync("cargo", args, { cwd: root, stdio: "inherit" });

const out = join(targetDir, triple === host ? "" : triple, debug ? "debug" : "release");
const bin = join(app, "src-tauri", "binaries");
mkdirSync(bin, { recursive: true });
for (const name of ["urma-node", "urma-node-control"]) {
  copyFileSync(join(out, name + exe), join(bin, `${name}-${triple}${exe}`));
}
