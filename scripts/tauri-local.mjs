// Wrapper the `tauri` npm script calls (see app/package.json) so a plain
// `npm run tauri build` / `npm run tauri dev` on a developer's Mac is signed with a stable local
// identity instead of ad-hoc.
//
// Why: macOS ties Screen Recording and Accessibility grants to the app's code signature. The
// committed config signs ad-hoc ("-"), which changes on every build, so each rebuild silently loses
// those grants (dictation stops pasting, meetings capture the mic only). Signing every local build
// with one self-signed certificate keeps the grants. Create it once (Keychain Access › Certificate
// Assistant › Create a Certificate, type "Code Signing") named "Wisp Local Dev", or set
// WISP_SIGNING_IDENTITY to your own name.
//
// This only acts on macOS when that identity is in the keychain, so CI, Windows/Linux, and machines
// without the certificate fall straight through to tauri's configured signing. It never overrides an
// APPLE_SIGNING_IDENTITY you set yourself — set APPLE_SIGNING_IDENTITY=- to force an ad-hoc build.

import { execFileSync, spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** The self-signed certificate name a local macOS build uses when WISP_SIGNING_IDENTITY is unset. */
export const DEFAULT_SIGNING_IDENTITY = "Wisp Local Dev";

/**
 * Decide the APPLE_SIGNING_IDENTITY to use for a local build. Pure so it can be tested without a
 * keychain: `hasIdentity(name)` reports whether a code-signing certificate of that name exists.
 *
 * Returns `{ identity, reason }`. `identity === null` means "leave the environment untouched" — tauri
 * then uses tauri.conf.json's signingIdentity (ad-hoc) or an APPLE_SIGNING_IDENTITY already in scope.
 */
export function chooseSigningIdentity({ platform, env, hasIdentity }) {
  if (platform !== "darwin") {
    return { identity: null, reason: "not macOS — using tauri's configured signing" };
  }
  if (env.APPLE_SIGNING_IDENTITY) {
    return {
      identity: null,
      reason: `APPLE_SIGNING_IDENTITY already set to "${env.APPLE_SIGNING_IDENTITY}" — leaving it`,
    };
  }
  const wanted = (env.WISP_SIGNING_IDENTITY ?? DEFAULT_SIGNING_IDENTITY).trim();
  if (!wanted) {
    return { identity: null, reason: "signing identity is empty — using tauri's configured signing" };
  }
  if (!hasIdentity(wanted)) {
    return {
      identity: null,
      reason: `"${wanted}" is not in the keychain — using tauri's configured signing (ad-hoc). ` +
        "Create it once (see scripts/tauri-local.mjs) so permissions survive rebuilds.",
    };
  }
  return { identity: wanted, reason: `signing with "${wanted}" so macOS permission grants persist` };
}

/** Whether a code-signing certificate named `name` exists in the keychain. */
function keychainHasIdentity(name) {
  try {
    const out = execFileSync("security", ["find-identity", "-p", "codesigning", "-v"], {
      encoding: "utf8",
    });
    return out.includes(`"${name}"`);
  } catch {
    // No `security` (not macOS) or it failed — treat as absent and fall through.
    return false;
  }
}

/** Absolute path to the real tauri CLI entry in the app's node_modules. */
function resolveTauriCli(appDir) {
  const cliDir = resolve(appDir, "node_modules", "@tauri-apps", "cli");
  const pkg = JSON.parse(readFileSync(resolve(cliDir, "package.json"), "utf8"));
  const binRel = typeof pkg.bin === "string" ? pkg.bin : pkg.bin.tauri;
  return resolve(cliDir, binRel);
}

function main() {
  const scriptDir = dirname(fileURLToPath(import.meta.url));
  const appDir = resolve(scriptDir, "..", "app");

  const decision = chooseSigningIdentity({
    platform: process.platform,
    env: process.env,
    hasIdentity: keychainHasIdentity,
  });
  // Only worth a line on macOS; elsewhere the wrapper is an invisible passthrough.
  if (process.platform === "darwin") {
    console.error(`tauri-local: ${decision.reason}`);
  }

  const env = { ...process.env };
  if (decision.identity) {
    env.APPLE_SIGNING_IDENTITY = decision.identity;
  }

  const cli = resolveTauriCli(appDir);
  const result = spawnSync(process.execPath, [cli, ...process.argv.slice(2)], {
    stdio: "inherit",
    env,
  });
  if (result.error) {
    console.error(`tauri-local: failed to run the tauri CLI: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status ?? 1);
}

// Run only when invoked as a script, not when imported by the test.
if (fileURLToPath(import.meta.url) === resolve(process.argv[1] ?? "")) {
  main();
}
