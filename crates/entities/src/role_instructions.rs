//! Behavioral instructions associated with each coordination role.
//!
//! Returned to a session both via the `set_role` tool result (for self-set
//! changes) and via a daemon-pushed `NotifyChannel` (when a peer or operator
//! changes the session's role).

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
            a role another session in this folder is already holding."
            .to_string(),

        other => format!(
            "Your role is now **{other}**. There are no built-in instructions for \
             this role yet — coordinate with peer sessions accordingly."
        ),
    }
}
