// Resolve the session-identity metadata the roster shows for this pi session:
// operator, host, git branch, and project basename. Mirrors the Rust shim's
// startup probes (main.rs::detect_*) and the opencode plugin's identity.ts,
// but runs in-process via node child_process instead of Bun's `$` shell.

import { execFileSync } from "node:child_process";

import type { HostInfo } from "./entities.ts";

export interface Identity {
  operator: string;
  host: HostInfo;
  gitBranch?: string;
  project?: string;
}

/** Map node's `process.platform` to the same `os` tokens the Rust shim reports
 *  (`std::env::consts::OS`), so a mixed Claude/opencode/pi roster reads
 *  uniformly. */
function normalizeOs(platform: string): string {
  switch (platform) {
    case "darwin": return "macos";
    case "win32":  return "windows";
    default:       return platform; // "linux" etc. already match
  }
}

function tryText(file: string, args: string[], cwd?: string): string | undefined {
  try {
    const out = execFileSync(file, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim();
    return out.length > 0 ? out : undefined;
  } catch {
    return undefined;
  }
}

export async function resolveIdentity(cwd: string): Promise<Identity> {
  // Operator: explicit override → unix `$USER` → windows `%USERNAME%` →
  // "anonymous" (same precedence as the Rust shim's detect_operator).
  const operator =
    process.env.MARSHAL_OPERATOR || process.env.USER || process.env.USERNAME || "anonymous";

  const hostnameRaw = tryText("hostname", []);
  // `hostname` can return an FQDN; the daemon's host:* auto-room keys on the
  // short name, so drop the domain (matches the daemon's own trimming).
  const name = (hostnameRaw ?? "unknown").split(".")[0] || "unknown";

  const host: HostInfo = {
    name,
    os: normalizeOs(process.platform),
    arch: process.arch,
  };

  let gitBranch = tryText("git", ["rev-parse", "--abbrev-ref", "HEAD"], cwd);
  if (gitBranch === "HEAD") gitBranch = undefined; // detached head → no branch

  const toplevel = tryText("git", ["rev-parse", "--show-toplevel"], cwd);
  const project = toplevel ? toplevel.split(/[/\\]/).pop() || undefined : undefined;

  return { operator, host, gitBranch, project };
}