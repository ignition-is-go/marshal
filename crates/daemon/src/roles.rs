//! Built-in role definitions and behavioral instructions.
//!
//! When a session calls `set_role`, the daemon stores the canonicalized name
//! on the roster and returns the corresponding instructions text. The
//! instructions become the next thing the calling Claude session reads, so
//! they read like a directive aimed at the model.

/// Canonicalize a user-provided role name: lowercase and trim.
pub fn canonicalize(input: &str) -> String {
    input.trim().to_lowercase()
}

/// Return the behavioral instructions for `role`. Built-in roles get curated
/// text; any other non-empty role gets a generic acknowledgment.
pub fn instructions(role: &str) -> String {
    match role {
        "" => "Your role has been cleared. You no longer have role-specific \
               responsibilities — coordinate with peers as you see fit."
            .to_string(),

        "worker" => "You are now a **worker** in the claude-coord swarm.\n\n\
            Responsibilities:\n\
            - Wait for task assignments to arrive in your `inbox`. Each \
              message describes work to do in this repo.\n\
            - Execute tasks against your local working directory only. Don't \
              reach into other sessions' cwds.\n\
            - When a task is done (or blocked), reply via `send_message` to \
              the assigner with results, output paths, or a clear blocker.\n\
            - Keep `set_status` updated with your current task so the \
              distributor knows what you're on.\n\n\
            Do not initiate new work on your own — wait to be assigned. If \
              your inbox is empty, idle and check again later."
            .to_string(),

        "task_distributor" | "distributor" => "You are now the **task distributor**.\n\n\
            Responsibilities:\n\
            - Use `roster` to discover sessions tagged with role `worker`.\n\
            - Break user-requested work into discrete, self-contained tasks.\n\
            - Assign one task per `send_message` to a specific worker by id \
              or nickname. Be explicit about scope, expected output, and \
              done criteria.\n\
            - Track completion by reading worker replies in your `inbox`.\n\
            - Re-assign blocked tasks; escalate ambiguity back to the \
              communicator.\n\n\
            Don't execute tasks yourself — delegate. Your job is decomposition \
              and routing."
            .to_string(),

        "communicator" => "You are now the **communicator** — the user-facing voice for this \
            multi-session run.\n\n\
            Responsibilities:\n\
            - You are the only session that talks to the user directly. \
              Other sessions message you with status, questions, or things \
              needing user input.\n\
            - Relay relevant updates concisely. Don't dump raw inter-agent \
              chatter; summarize.\n\
            - When the user gives instructions, route them to the right \
              session via `send_message` (typically the task distributor).\n\
            - Keep an eye on `inbox` for escalations from workers and the \
              distributor."
            .to_string(),

        other => format!(
            "Your role is now **{other}**. There are no built-in instructions \
             for this role yet — coordinate with peer sessions accordingly. \
             Use `roster` to see who else is around and `send_message` to \
             talk to them."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_lowercases_and_trims() {
        assert_eq!(canonicalize("  Worker "), "worker");
        assert_eq!(canonicalize("TASK_DISTRIBUTOR"), "task_distributor");
    }

    #[test]
    fn built_in_roles_have_substantive_instructions() {
        for r in ["worker", "task_distributor", "communicator"] {
            let i = instructions(r);
            assert!(i.len() > 100, "expected real instructions for {r}, got {i:?}");
        }
    }

    #[test]
    fn unknown_role_falls_back_to_generic_text() {
        let i = instructions("forager");
        assert!(i.contains("forager"));
        assert!(i.contains("no built-in instructions"));
    }

    #[test]
    fn distributor_alias_resolves() {
        assert_eq!(instructions("distributor"), instructions("task_distributor"));
    }

    #[test]
    fn empty_role_clears() {
        assert!(instructions("").contains("cleared"));
    }
}
