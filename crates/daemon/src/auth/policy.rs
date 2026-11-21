use myko_auth::{
    AuthError, AuthenticatedIdentity, AuthorizationRequest, Authorizer, OperationKind,
};

/// Public coordination preserves the existing trusted-agent contract. Verified
/// operator console identities can only be created through their own command.
pub(super) struct MarshalPolicy;
impl Authorizer for MarshalPolicy {
    fn authorize_anonymous(&self, request: AuthorizationRequest<'_>) -> Result<(), AuthError> {
        // Myko separately checks ownership of outstanding command responses.
        if matches!(request, AuthorizationRequest::CommandResult { .. }) {
            return Ok(());
        }
        myko_auth::public_access::authorize(request)?;
        protect_operator_identity(None, request)
    }
    fn authorize(
        &self,
        identity: &AuthenticatedIdentity,
        request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        protect_operator_identity(Some(identity), request)?;
        if myko_auth::public_access::authorize(request).is_ok() {
            return Ok(());
        }
        let allowed = match request {
            AuthorizationRequest::Discover { kind, operation }
            | AuthorizationRequest::Execute {
                kind, operation, ..
            } => match kind {
                OperationKind::Query => operation == "GetAllSandboxRuns",
                OperationKind::Report => matches!(operation, "WatchUsers" | "WatchPresenceRoom"),
                OperationKind::Command => matches!(
                    operation,
                    "RegisterOperatorConsole"
                        | "CloseOperatorConsole"
                        | "ImportSandboxRun"
                        | "JoinPresenceRoom"
                        | "LeavePresenceRoom"
                ),
                OperationKind::View => false,
            },
            AuthorizationRequest::CommandResult { .. } => true,
            AuthorizationRequest::ClientEvent { .. } => false,
        };
        allowed.then_some(()).ok_or(AuthError::Forbidden)
    }
}

fn protect_operator_identity(
    identity: Option<&AuthenticatedIdentity>,
    request: AuthorizationRequest<'_>,
) -> Result<(), AuthError> {
    let (id, may_act_as_console) = match request {
        AuthorizationRequest::ClientEvent { proposed, .. } => {
            (proposed.get("id").and_then(|v| v.as_str()), false)
        }
        AuthorizationRequest::Execute {
            operation,
            arguments,
            ..
        } => match operation {
            "RegisterSession" => (
                arguments.pointer("/session/id").and_then(|v| v.as_str()),
                false,
            ),
            "SendMessage" | "BroadcastMessage" | "JoinRoom" | "LeaveRoom" | "ReadMessages"
            | "AckMessages" => (arguments.get("asSession").and_then(|v| v.as_str()), true),
            "RegisterOperatorConsole" => (
                arguments.pointer("/session/id").and_then(|v| v.as_str()),
                true,
            ),
            _ => (
                arguments.get("id").and_then(|v| v.as_str()),
                operation == "CloseOperatorConsole",
            ),
        },
        _ => (None, false),
    };
    let Some(id) = id.filter(|id| marshal_entities::is_operator_console(id)) else {
        return Ok(());
    };
    if may_act_as_console
        && identity
            .and_then(|identity| identity.user_id.as_ref())
            .is_some_and(|user| {
                id.starts_with(&marshal_entities::operator_console_prefix(user.as_ref()))
            })
    {
        return Ok(());
    }
    Err(AuthError::Forbidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_agent_contract_excludes_verified_operator_commands() {
        marshal_entities::link();
        assert!(
            MarshalPolicy
                .authorize_anonymous(AuthorizationRequest::CommandResult {
                    transaction: "owned-by-transport"
                })
                .is_ok()
        );
        for operation in [
            "RegisterSession",
            "SendMessage",
            "ReadMessages",
            "JoinRoom",
            "LeaveRoom",
            "SetSessionLastActivityAt",
        ] {
            assert!(
                MarshalPolicy
                    .authorize_anonymous(AuthorizationRequest::Execute {
                        kind: OperationKind::Command,
                        operation,
                        arguments: &json!({"id":"agent","asSession":"agent"})
                    })
                    .is_ok(),
                "{operation}"
            );
        }
        for operation in [
            "RegisterOperatorConsole",
            "CloseOperatorConsole",
            "ImportSandboxRun",
        ] {
            assert!(
                MarshalPolicy
                    .authorize_anonymous(AuthorizationRequest::Execute {
                        kind: OperationKind::Command,
                        operation,
                        arguments: &json!({})
                    })
                    .is_err(),
                "{operation}"
            );
        }
    }

    #[test]
    fn anonymous_wake_bridge_can_observe_read_acknowledgements() {
        marshal_entities::link();
        for operation in ["GetAllSessions", "GetAllMessages", "GetAllMessageReads"] {
            assert!(
                MarshalPolicy
                    .authorize_anonymous(AuthorizationRequest::Execute {
                        kind: OperationKind::Query,
                        operation,
                        arguments: &json!({}),
                    })
                    .is_ok(),
                "wake bridge query {operation} must remain available to native agents"
            );
        }
    }

    #[test]
    fn anonymous_agents_cannot_claim_verified_console_identity() {
        for (operation, arguments) in [
            (
                "RegisterSession",
                json!({"session":{"id":"console-user:tab"}}),
            ),
            ("SendMessage", json!({"asSession":"console-user:tab"})),
            ("DeleteSession", json!({"id":"console-user:tab"})),
            ("SetSessionLastActivityAt", json!({"id":"console-user:tab"})),
        ] {
            assert!(
                MarshalPolicy
                    .authorize_anonymous(AuthorizationRequest::Execute {
                        kind: OperationKind::Command,
                        operation,
                        arguments: &arguments
                    })
                    .is_err(),
                "{operation}"
            );
        }
    }
}
