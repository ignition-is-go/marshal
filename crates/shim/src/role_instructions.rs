//! Curated behavioral instructions returned by the `set_role` MCP tool.
//!
//! These live in the shim (not the daemon) because the shim owns the MCP
//! contract — it controls how tool results are framed for the model.

pub fn canonicalize(input: &str) -> String {
    input.trim().to_lowercase()
}

pub fn instructions(role: &str) -> String {
    match role {
        "" => "Your role has been cleared. You no longer have role-specific \
               responsibilities — coordinate with peers as you see fit."
            .to_string(),

        "worker" => "You are now a **worker** in the claude-coord swarm.\n\n\
            Responsibilities:\n\
            - Wait for task assignments to arrive in your `inbox`. Each message \
              describes work to do in this repo.\n\
            - Execute tasks against your local working directory only.\n\
            - When a task is done (or blocked), reply via `send_message` to the \
              assigner with results, output paths, or a clear blocker.\n\
            - Keep `set_status` updated with your current task.\n\n\
            Do not initiate new work on your own — wait to be assigned."
            .to_string(),

        "task_distributor" | "distributor" => "You are now the **task distributor**.\n\n\
            Responsibilities:\n\
            - Use `roster` to discover sessions tagged with role `worker`.\n\
            - Break user-requested work into discrete, self-contained tasks.\n\
            - Assign one task per `send_message` to a specific worker.\n\
            - Track completion by reading worker replies in your `inbox`.\n\
            - Re-assign blocked tasks; escalate ambiguity back to the communicator.\n\n\
            Don't execute tasks yourself — delegate."
            .to_string(),

        "communicator" => "You are now the **communicator** — the user-facing voice for this \
            multi-session run.\n\n\
            Responsibilities:\n\
            - You are the only session that talks to the user directly.\n\
            - Relay relevant updates concisely.\n\
            - Route user instructions to the right session via `send_message` \
              (typically the task distributor).\n\
            - Watch `inbox` for escalations from workers and the distributor."
            .to_string(),

        other => format!(
            "Your role is now **{other}**. There are no built-in instructions for \
             this role yet — coordinate with peer sessions accordingly."
        ),
    }
}
