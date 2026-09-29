#!/usr/bin/env bash
# Builds Wisp.app for this Mac, signed with a stable local identity.
#
# macOS ties Screen Recording and Microphone grants to the app's signature. The committed config
# signs ad hoc ("-"), which changes on every build, so each rebuild loses those grants. Signing with
# one self-signed certificate keeps them. Create it once in Keychain Access (Certificate Assistant ›
# Create a Certificate…, type "Code Signing") and name it as below, or set WISP_SIGNING_IDENTITY.
set -euo pipefail

identity="${WISP_SIGNING_IDENTITY:-Wisp Local Dev}"
if ! security find-identity -p codesigning -v | grep -q "\"$identity\""; then
  echo "No valid code-signing identity named \"$identity\" in your keychain." >&2
  exit 1
fi

cd "$(dirname "$0")/../app"
APPLE_SIGNING_IDENTITY="$identity" npx tauri build --bundles app "$@"
