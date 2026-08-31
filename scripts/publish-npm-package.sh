#!/usr/bin/env bash
set -euo pipefail

package_name=$(node --print 'require("./package.json").name')
package_version=$(node --print 'require("./package.json").version')
error_log=$(mktemp)
trap 'rm -f "$error_log"' EXIT

if published_version=$(npm view "${package_name}@${package_version}" version --json 2>"$error_log"); then
  published_version=${published_version//\"/}
  if [[ "$published_version" != "$package_version" ]]; then
    echo "npm returned unexpected version for ${package_name}: ${published_version}" >&2
    exit 1
  fi
  echo "${package_name} ${package_version} already exists on npm; skipping"
elif grep -q 'E404' "$error_log"; then
  # npm provenance currently rejects self-hosted GitHub Actions runners. OIDC
  # trusted publishing still authenticates this job; publish without provenance.
  npm publish --access public
else
  cat "$error_log" >&2
  exit 1
fi
