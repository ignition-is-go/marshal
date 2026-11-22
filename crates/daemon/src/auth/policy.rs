use myko_auth::{AuthError, AuthenticatedIdentity, AuthorizationRequest, Authorizer};

/// Both relay listeners expose the trusted agent operation set.
pub(super) struct MarshalPolicy;

impl Authorizer for MarshalPolicy {
    fn authorize_anonymous(&self, request: AuthorizationRequest<'_>) -> Result<(), AuthError> {
        // Myko separately checks ownership of outstanding command responses.
        if matches!(request, AuthorizationRequest::CommandResult { .. }) {
            return Ok(());
        }
        myko_auth::public_access::authorize(request)?;
        protect_reserved_session_ids(request)
    }

    fn authorize(
        &self,
        _: &AuthenticatedIdentity,
        request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        self.authorize_anonymous(request)
    }
}

fn protect_reserved_session_ids(request: AuthorizationRequest<'_>) -> Result<(), AuthError> {
    let id = match request {
        AuthorizationRequest::ClientEvent { proposed, .. } => {
            proposed.get("id").and_then(|value| value.as_str())
        }
        AuthorizationRequest::Execute {
            operation,
            arguments,
            ..
        } => match operation {
            "RegisterSession" => arguments
                .pointer("/session/id")
                .and_then(|value| value.as_str()),
            "SendMessage" | "BroadcastMessage" | "JoinRoom" | "LeaveRoom" | "ReadMessages"
            | "AckMessages" => arguments.get("asSession").and_then(|value| value.as_str()),
            _ => arguments.get("id").and_then(|value| value.as_str()),
        },
        _ => None,
    };
    if id.is_some_and(marshal_entities::is_operator_console) {
        return Err(AuthError::Forbidden);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use myko_auth::OperationKind;
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
