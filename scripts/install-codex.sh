#!/bin/sh
# marshal — one-line Codex installer for macOS / Linux.
#
#   curl -fsSL https://github.com/ignition-is-go/marshal/releases/latest/download/install-codex.sh \
#     | sh -s -- --daemon ws://<daemon-host>:6155
#
# Detects this machine's OS/arch, downloads the matching marshal-shim from the
# latest release, and runs `codex-setup` — which installs the shim to a stable
# per-user path, wires ~/.codex (marshal MCP server + hooks), and pre-trusts the
# hooks so they fire in the ChatGPT desktop app. Windows users run the .exe with
# `codex-setup` directly (see the release notes).
set -eu

REPO="https://github.com/ignition-is-go/marshal"
DAEMON="${MARSHAL_DAEMON:-}"
while [ $# -gt 0 ]; do
    case "$1" in
        --daemon) DAEMON="${2:-}"; shift 2 ;;
        -h | --help)
            echo "usage: install-codex.sh --daemon ws://host:6155"
            exit 0
            ;;
        *)
            echo "install-codex: unknown argument '$1'" >&2
            exit 1
            ;;
    esac
done
if [ -z "$DAEMON" ]; then
    echo "error: pass the marshal daemon address, e.g." >&2
    echo "  ... | sh -s -- --daemon ws://marshal-01.example:6155" >&2
    exit 1
fi

os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Darwin)
        case "$arch" in
            arm64) target="aarch64-apple-darwin" ;;
            x86_64) target="x86_64-apple-darwin" ;;
            *) echo "unsupported macOS arch: $arch" >&2; exit 1 ;;
        esac
        ;;
    Linux) target="x86_64-unknown-linux-gnu" ;;
    *)
        echo "unsupported OS: $os — on Windows, run the .exe with 'codex-setup'" >&2
        exit 1
        ;;
esac

url="$REPO/releases/latest/download/marshal-shim-$target"
tmp=$(mktemp -d)
bin="$tmp/marshal-shim"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading marshal-shim ($target)..."
curl -fSL --proto '=https' --tlsv1.2 -o "$bin" "$url"
chmod +x "$bin"
# Clear the Gatekeeper quarantine in case this binary was fetched via a browser
# (curl downloads carry none). Harmless on Linux.
xattr -d com.apple.quarantine "$bin" 2>/dev/null || true

echo "Wiring marshal into Codex..."
"$bin" codex-setup --daemon "$DAEMON"
