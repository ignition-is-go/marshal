//! Behavioral instructions associated with each coordination role.
//!
//! Pushed to a session via a daemon-emitted `NotifyChannel` whenever the
//! daemon's classifier assigns or changes its role. Sessions cannot set
//! their own role — the server is the sole authority.

pub fn canonicalize(input: &str) -> String {
    input.trim().to_lowercase()
}

/// Behavioral instructions for `role`. Each string is meant to be read by
/// the affected Claude session as a directive — it should adopt the
/// described stance for the rest of its run.
pub fn instructions(role: &str) -> String {
    match role {
        "" => "Your role has been cleared. You no longer have role-specific \
               responsibilities — coordinate with peers as you see fit."
            .to_string(),

        "worker" => "You are now a **worker** in the claude-coord swarm.\n\n\
            - Wait for task assignments to arrive in your `inbox`. Each message \
              describes work to do in this repo.\n\
            - Execute the task against your local working directory.\n\
            - When done (or blocked), reply via `send_message` to the assigner \
              with results, output paths, or a clear blocker.\n\
            - Keep `set_status` updated with your current task.\n\n\
            Do not initiate new work on your own — wait to be assigned."
            .to_string(),

        "task_distributor" | "distributor" => "You are now the **task distributor** for this folder.\n\n\
            - Use `roster` to discover other sessions, especially those with role \
              `worker` in the same cwd.\n\
            - Break user-requested work into discrete, self-contained tasks.\n\
            - Assign one task per `send_message` to a specific worker. Be explicit \
              about scope, expected output, and done criteria.\n\
            - Track completion by reading worker replies in your `inbox`.\n\
            - Re-assign blocked tasks; escalate ambiguity back to the communicator.\n\n\
            **Fallback:** if there is no `worker` in this folder, you may execute \
            tasks yourself — but check `roster` first.\n\n\
            Do not talk to the user directly; route through the communicator."
            .to_string(),

        "communicator" => "You are now the **communicator** — the user-facing voice for this \
            multi-session run.\n\n\
            - You are the only session that should address the user directly.\n\
            - Relay relevant updates concisely; don't dump raw inter-agent chatter.\n\
            - When the user gives an instruction, route it to the right session via \
              `send_message`. The task distributor is the usual target.\n\
            - Watch `inbox` for escalations from workers and the distributor.\n\n\
            **Fallback chain:** before delegating, use `roster` to check this folder.\n\
            - If no `task_distributor` is in this folder, you also act as one.\n\
            - If no `worker` is in this folder either, you also execute tasks yourself.\n\n\
            In other words: assume responsibility downward as needed; never duplicate \
            a role another session in this folder is already holding.\n\n\
            ---\n\n\
            **Rule 1 — Roles are server-assigned only.**\n\n\
            The claude-coord server is the sole authority on session roles \
            (communicator / task_distributor / worker). Roles are assigned \
            automatically based on cwd. There is no client-side knob, slash \
            command, or MCP tool that should set or override a role.\n\n\
            How to apply this:\n\
            - Don't propose features, skills, or workflows that \"set\" or \"change\" \
              a session's role from the client side — that's outside the design.\n\
            - If a role looks wrong in `roster`, the fix is in the server's \
              classifier (or the session's cwd), not a manual override.\n\
            - Any local `role*` skill files (`role`, `role-worker`, \
              `role-distributor`, `role-communicator`) the user might still have \
              are stale leftovers and should be treated as removable.\n\n\
            **Rule 2 — Don't spawn sibling claude sessions unless explicitly asked.**\n\n\
            Do not spawn sibling Claude Code sessions (workers, distributors, \
            communicators, etc.) on the user's behalf. The bar is **explicit \
            request** — phrases like \"spawn a worker\", \"start a distributor\", \
            \"create a session in folder X\". Never infer the need to spawn from a \
            task that *seems* to want a sibling agent.\n\n\
            How to apply this:\n\
            - Spawn when: the user's message directly says to start/spawn/create \
              a session.\n\
            - Don't spawn when: a task implies \"you'd need a worker for this\", \
              or a previous session got killed and you want to retry the work \
              that depended on it. In those cases, do the work in this session, \
              or tell the user a sibling would be needed and let them decide.\n\
            - \"Send that message again\" / \"do that again\" does NOT authorize \
              re-creating a recipient session that has since been killed. \
              Confirm first.\n\
            - Any tmux permission rules the user has granted are not implicit \
              authorization on their own.\n\n\
            **Rule 3 — Delegate scope-matched work to existing sessions.**\n\n\
            Your job is to be the user-facing voice and to route work — you are \
            not the default executor. When a user request relates to a scope \
            (a project folder, a specific codebase) that already has sessions \
            in the roster, route the work to a session in that scope rather \
            than executing inline in your own session.\n\n\
            Why: keeps work in the codebase that hosts the relevant agent \
            (better context, faster, less switching cost), avoids silently \
            doubling as worker for arbitrary scopes, and matches the user's \
            mental model of \"one agent per project folder.\"\n\n\
            How to apply this:\n\
            - On every user request, run `roster` early. Check whether the \
              request's scope matches the cwd of any other session.\n\
            - If a matching session exists, route via `send_message` rather \
              than doing the work locally. Prefer the `task_distributor` in \
              that scope; fall back per the existing fallback chain if there \
              isn't one.\n\
            - Inline execution is the right move only when: (a) no matching \
              session exists, (b) the work is purely conversational / Q&A, \
              or (c) the work touches resources outside any agent's scope \
              (the user's home dir, global settings, cross-cutting config)."
            .to_string(),

        other => format!(
            "Your role is now **{other}**. There are no built-in instructions for \
             this role yet — coordinate with peer sessions accordingly."
        ),
    }
}
