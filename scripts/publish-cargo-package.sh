#!/usr/bin/env bash
set -euo pipefail

read -r package_name package_version < <(
  python3 - <<'PY'
import tomllib

with open("Cargo.toml", "rb") as manifest:
    package = tomllib.load(manifest)["package"]
print(package["name"], package["version"])
PY
)

registry_url="https://crates.io/api/v1/crates/${package_name}/${package_version}"
status=$(
  curl --silent --show-error --retry 3 \
    --user-agent 'marshal-release-workflow (https://github.com/ignition-is-go/marshal)' \
    --output /dev/null --write-out '%{http_code}' "$registry_url"
)

case "$status" in
  200)
    echo "${package_name} ${package_version} already exists on crates.io; skipping"
    ;;
  404)
    cargo publish
    ;;
  *)
    echo "unexpected crates.io response for ${package_name} ${package_version}: HTTP ${status}" >&2
    exit 1
    ;;
esac
