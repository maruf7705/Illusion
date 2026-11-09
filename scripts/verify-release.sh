#!/usr/bin/env bash
# Release gate for the browser capture protocol and native downloader.
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

"${project_root}/scripts/sync-firefox-extension.sh"
cd "${project_root}"
cargo check --workspace
cargo test --workspace
