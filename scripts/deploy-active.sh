#!/usr/bin/env bash
# Deploy a verified release only to Illusion's active user-level runtime
# locations.  This deliberately avoids the desktop/icon/Hyprland changes made
# by the broader installer.
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
release_dir="${project_root}/target/release"
xpi_source="${project_root}/extension-firefox/illusion-downloader.xpi"
bin_dir="${HOME}/.local/bin"
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
backup_dir="${HOME}/.local/share/illusion/backups/${timestamp}"
mkdir -p "${backup_dir}"

for name in illusion illusion-cli illusion-native-host; do
  if [[ ! -x "${release_dir}/${name}" ]]; then
    echo "Missing release artifact: ${release_dir}/${name}" >&2
    exit 1
  fi
done
if [[ ! -f "${xpi_source}" ]]; then
  echo "Missing Firefox package: ${xpi_source}" >&2
  exit 1
fi

backup_index=0
install_atomically() {
  local source=$1
  local destination=$2
  local mode=$3
  local temp

  mkdir -p "$(dirname "${destination}")"
  if [[ -e "${destination}" ]]; then
    backup_index=$((backup_index + 1))
    cp -p -- "${destination}" "${backup_dir}/${backup_index}-$(basename "${destination}")"
  fi
  temp=$(mktemp "${destination}.tmp.XXXXXX")
  install -m "${mode}" -- "${source}" "${temp}"
  mv -f -- "${temp}" "${destination}"
}

for name in illusion illusion-cli illusion-native-host; do
  install_atomically "${release_dir}/${name}" "${bin_dir}/${name}" 755
done

xpi_targets=()
for root in "${HOME}/.config/zen" "${HOME}/.mozilla/firefox"; do
  [[ -d "${root}" ]] || continue
  while IFS= read -r -d '' target; do
    xpi_targets+=("${target}")
  done < <(find "${root}" -type f -path '*/extensions/illusion@archlinux.org.xpi' -print0)
done

for target in "${xpi_targets[@]}"; do
  install_atomically "${xpi_source}" "${target}" 644
done

if [[ ${#xpi_targets[@]} -eq 0 ]]; then
  echo "No installed Firefox/Zen Illusion XPI was found; package is ready at ${xpi_source}." >&2
fi

echo "Deployed Illusion binaries and ${#xpi_targets[@]} Firefox/Zen package(s)."
echo "Backups: ${backup_dir}"
echo "Restart Illusion and reload the unpacked Chromium extension to activate the new code."
