// Unit tests for the signing-identity decision in tauri-local.mjs. Run with `node --test scripts`.
//
// These pin the invariants that keep the wrapper safe: it must be a no-op on CI and non-macOS (so it
// never changes those builds), must never override an identity the caller set, and must honour a
// custom identity name.

import assert from "node:assert/strict";
import { test } from "node:test";

import { chooseSigningIdentity, DEFAULT_SIGNING_IDENTITY } from "./tauri-local.mjs";

const present = () => true;
const absent = () => false;

test("macOS with the certificate present → sets the default identity", () => {
  const d = chooseSigningIdentity({ platform: "darwin", env: {}, hasIdentity: present });
  assert.equal(d.identity, DEFAULT_SIGNING_IDENTITY);
});

test("non-macOS → leaves the environment untouched", () => {
  for (const platform of ["win32", "linux"]) {
    const d = chooseSigningIdentity({ platform, env: {}, hasIdentity: present });
    assert.equal(d.identity, null);
  }
});

test("certificate absent (e.g. CI, a fresh checkout) → leaves the environment untouched", () => {
  const d = chooseSigningIdentity({ platform: "darwin", env: {}, hasIdentity: absent });
  assert.equal(d.identity, null);
});

test("APPLE_SIGNING_IDENTITY already set is respected, not overridden", () => {
  // The ad-hoc opt-out: a caller forcing ad-hoc with "-" keeps it.
  const adhoc = chooseSigningIdentity({
    platform: "darwin",
    env: { APPLE_SIGNING_IDENTITY: "-" },
    hasIdentity: present,
  });
  assert.equal(adhoc.identity, null);

  // A real Developer ID set by a release build is left alone.
  const releaseEnv = { APPLE_SIGNING_IDENTITY: "Developer ID Application: Example (TEAMID)" };
  const release = chooseSigningIdentity({ platform: "darwin", env: releaseEnv, hasIdentity: present });
  assert.equal(release.identity, null);
});

test("WISP_SIGNING_IDENTITY overrides the default name and is the only name probed", () => {
  const probed = [];
  const d = chooseSigningIdentity({
    platform: "darwin",
    env: { WISP_SIGNING_IDENTITY: "My Cert" },
    hasIdentity: (name) => {
      probed.push(name);
      return name === "My Cert";
    },
  });
  assert.equal(d.identity, "My Cert");
  assert.deepEqual(probed, ["My Cert"]);
});

test("a blank WISP_SIGNING_IDENTITY falls through instead of probing an empty name", () => {
  const probed = [];
  const d = chooseSigningIdentity({
    platform: "darwin",
    env: { WISP_SIGNING_IDENTITY: "   " },
    hasIdentity: (name) => {
      probed.push(name);
      return true;
    },
  });
  assert.equal(d.identity, null);
  assert.deepEqual(probed, []);
});
