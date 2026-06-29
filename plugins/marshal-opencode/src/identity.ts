// Resolve the session-identity metadata the roster shows for this opencode
// session: operator, host, git branch, and project basename. Mirrors the Rust
// shim's startup probes (main.rs::detect_*), but runs in-process via opencode's
// Bun `$` shell instead of std::process.

import type { PluginInput } from "@opencode-ai/plugin";

import type { HostInfo } from "./entities.js";

export interface Identity {
  operator: string;
  host: HostInfo;
  gitBranch?: string;
  project?: string;
}

/** Bun's `$` shell exactly as handed to the plugin in its context. */
type Shell = PluginInput["$"];

/** Map Bun's `process.platform` to the same `os` tokens the Rust shim reports
 *  (`std::env::consts::OS`), so a mixed Claude/opencode roster reads uniformly. */
function normalizeOs(platform: string): string {
  switch (platform) {
    case "darwin":
      return "macos";
    case "win32":
      return "windows";
    default:
      return platform; // "linux" etc. already match
  }
}

async function tryText($: Shell, run: () => ReturnType<Shell>): Promise<string | undefined> {
  try {
    const out = (await run().text()).trim();
    return out.length > 0 ? out : undefined;
  } catch {
    return undefined;
  }
}

export async function resolveIdentity($: Shell, cwd: string): Promise<Identity> {
  // Operator: explicit override → unix `$USER` → windows `%USERNAME%` →
  // "anonymous" (same precedence as the Rust shim's detect_operator).
  const operator =
    process.env.MARSHAL_OPERATOR || process.env.USER || process.env.USERNAME || "anonymous";

  const hostnameRaw = await tryText($, () => $`hostname`);
  // `hostname` can return an FQDN; the daemon's host:* auto-room keys on the
  // short name, so drop the domain (matches the daemon's own trimming).
  const name = (hostnameRaw ?? "unknown").split(".")[0] || "unknown";

  const host: HostInfo = {
    name,
    os: normalizeOs(process.platform),
    arch: process.arch,
  };

  let gitBranch = await tryText($, () => $`git -C ${cwd} rev-parse --abbrev-ref HEAD`);
  if (gitBranch === "HEAD") gitBranch = undefined; // detached head → no branch

  const toplevel = await tryText($, () => $`git -C ${cwd} rev-parse --show-toplevel`);
  const project = toplevel ? toplevel.split(/[/\\]/).pop() || undefined : undefined;

  return { operator, host, gitBranch, project };
}
