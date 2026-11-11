#!/usr/bin/env bash
set -euo pipefail

destination=${1:?usage: install-actionlint.sh DESTINATION}
archive=$(mktemp)
trap 'rm -f "$archive"' EXIT
curl --fail --silent --show-error --location --retry 3 --max-time 60 \
  https://github.com/rhysd/actionlint/releases/download/v1.7.12/actionlint_1.7.12_linux_amd64.tar.gz \
  --output "$archive"
echo "8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8  $archive" | sha256sum --check
mkdir -p "$destination"
tar -xzf "$archive" -C "$destination" actionlint
