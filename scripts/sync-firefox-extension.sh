#!/usr/bin/env bash
# Regenerate the Firefox extension and XPI from the canonical extension
# implementation.  Browser-specific manifest files remain in their own tree.
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source_root="${project_root}/extension"
firefox_root="${project_root}/extension-firefox"

shared_files=(
  "background.js"
  "content/floating_bar_ui.js"
  "content/mse_hook_main_world.js"
  "content/video_detector.js"
)

for relative_path in "${shared_files[@]}"; do
  mkdir -p "${firefox_root}/$(dirname "${relative_path}")"
  cp "${source_root}/${relative_path}" "${firefox_root}/${relative_path}"
done

for root in "${source_root}" "${firefox_root}"; do
  while IFS= read -r -d '' script; do
    node --check "${script}"
  done < <(find "${root}" -type f -name '*.js' -print0)
  node -e 'JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))' "${root}/manifest.json"
done

xpi_tmp=$(mktemp "${firefox_root}/.illusion-downloader.XXXXXX.xpi")
trap 'rm -f "${xpi_tmp}"' EXIT
(
  cd "${firefox_root}"
  find . -type f \
    ! -name 'illusion-downloader.xpi' \
    ! -name "$(basename "${xpi_tmp}")" \
    -print | bsdtar --format=zip -cf "${xpi_tmp}" -T -
)
unzip -tqq "${xpi_tmp}"
mv "${xpi_tmp}" "${firefox_root}/illusion-downloader.xpi"
trap - EXIT
