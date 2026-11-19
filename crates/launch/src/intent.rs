use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::ProcessRequest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchContext {
    pub daemon_address: String,
    pub hook_base: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchIntent {
    Process {
        harness: String,
        cwd: String,
        options: Vec<String>,
    },
    Managed(ManagedIntent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedIntent {
    Pulse {
        environment_id: String,
        preset_id: String,
        workspace: PulseWorkspace,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "provider",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PulseWorkspace {
    Session {},
    PulseRoot {},
    ExistingDirectory {
        path: String,
        display_name: Option<String>,
    },
    Pulse {
        repositories: Vec<String>,
        identity: WorkspaceIdentity,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum WorkspaceIdentity {
    Name { name: String },
    LeviTask { task_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackendIntent {
    Process {
        harness: String,
        argv: Vec<String>,
        cwd: String,
        env: BTreeMap<String, String>,
        options: Vec<String>,
    },
    Managed(ManagedIntent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub version: u32,
    pub name: String,
    #[serde(rename = "launch_id")]
    pub agent_id: String,
    pub marshal_version: String,
    pub context: LaunchContext,
    pub intent: BackendIntent,
}

impl From<ProcessRequest> for LaunchRequest {
    fn from(mut request: ProcessRequest) -> Self {
        let context = LaunchContext {
            daemon_address: request
                .env
                .remove("MARSHAL_DAEMON_ADDRESS")
                .unwrap_or_default(),
            hook_base: request.env.remove("MARSHAL_BASE_URL"),
        };
        Self {
            version: crate::PROTOCOL_VERSION,
            name: request.name,
            agent_id: request.agent_id,
            marshal_version: request.marshal_version,
            context,
            intent: BackendIntent::Process {
                harness: request.harness,
                argv: request.argv,
                cwd: request.cwd,
                env: request.env,
                options: request.options,
            },
        }
    }
}

impl LaunchRequest {
    pub fn process_request(&self) -> Result<ProcessRequest> {
        let BackendIntent::Process {
            harness,
            argv,
            cwd,
            env,
            options,
        } = &self.intent
        else {
            bail!("this backend accepts only process launch intent");
        };
        let mut env = env.clone();
        env.remove("MARSHAL_DAEMON_ADDRESS");
        env.remove("MARSHAL_BASE_URL");
        if !self.context.daemon_address.is_empty() {
            env.insert(
                "MARSHAL_DAEMON_ADDRESS".into(),
                self.context.daemon_address.clone(),
            );
        }
        if let Some(base) = &self.context.hook_base {
            env.insert("MARSHAL_BASE_URL".into(), base.clone());
        }
        Ok(ProcessRequest {
            name: self.name.clone(),
            agent_id: self.agent_id.clone(),
            marshal_version: self.marshal_version.clone(),
            harness: harness.clone(),
            argv: argv.clone(),
            cwd: cwd.clone(),
            env,
            options: options.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LaunchBackend;
    use serde_json::json;

    fn managed() -> serde_json::Value {
        json!({
            "version": crate::PROTOCOL_VERSION,
            "name": "agent-work",
            "launch_id": "launch-0123456789abcdef0123456789abcdef",
            "marshal_version": crate::MARSHAL_VERSION,
            "context": {"daemon_address": "ws://marshal.mesh:6155", "hook_base": "http://marshal.mesh:6156"},
            "intent": {"kind": "managed", "provider": "pulse", "environment_id": "development", "preset_id": "codex-account", "workspace": {"provider": "session"}}
        })
    }

    #[test]
    fn managed_intent_round_trips_without_execution_or_task_fields() {
        let wire = managed();
        let request: LaunchRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        assert!(
            request
                .process_request()
                .unwrap_err()
                .to_string()
                .contains("only process")
        );
        assert!(
            crate::tmux::Tmux::new("MUST_NOT_EXECUTE")
                .recovery_handle(&request)
                .is_err()
        );
    }

    #[test]
    fn managed_intent_rejects_process_fields_unknown_providers_and_tasks() {
        for (key, value) in [
            ("argv", json!(["sh"])),
            ("env", json!({})),
            ("cwd", json!("/tmp")),
            ("options", json!([])),
            ("first_task", json!("do work")),
        ] {
            let mut wire = managed();
            wire["intent"][key] = value;
            assert!(
                serde_json::from_value::<LaunchRequest>(wire).is_err(),
                "accepted {key}"
            );
        }
        let mut wire = managed();
        wire["intent"]["provider"] = json!("unknown");
        assert!(serde_json::from_value::<LaunchRequest>(wire).is_err());
        let mut wire = managed();
        wire["first_task"] = json!("do work");
        assert!(serde_json::from_value::<LaunchRequest>(wire).is_err());
    }

    #[test]
    fn first_task_belongs_to_the_api_request_only() {
        let wire = json!({
            "launch_id": "launch-0123456789abcdef0123456789abcdef", "backend": "pulse",
            "name": "agent-work", "check": true, "timeout_secs": 60,
            "first_task": "Run the failing test", "intent": managed()["intent"]
        });
        let request: crate::api::StartLaunch = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(request.first_task.as_deref(), Some("Run the failing test"));
        assert_eq!(serde_json::to_value(request).unwrap(), wire);
    }

    #[test]
    fn process_coordination_comes_from_the_common_context() {
        let mut wire = managed();
        wire["context"]["hook_base"] = serde_json::Value::Null;
        wire["intent"] = json!({
            "kind": "process", "harness": "claude", "argv": ["claude"], "cwd": "/work", "options": [],
            "env": {"MARSHAL_DAEMON_ADDRESS": "ws://wrong:6155", "MARSHAL_BASE_URL": "http://wrong:6156"}
        });
        let request: LaunchRequest = serde_json::from_value(wire).unwrap();
        let process = request.process_request().unwrap();
        assert_eq!(
            process.env["MARSHAL_DAEMON_ADDRESS"],
            "ws://marshal.mesh:6155"
        );
        assert!(!process.env.contains_key("MARSHAL_BASE_URL"));
    }

    #[test]
    fn api_process_intent_rejects_execution_and_task_fields() {
        let wire = json!({
            "launch_id": "launch-0123456789abcdef0123456789abcdef", "backend": "tmux",
            "name": "process", "check": true, "timeout_secs": 60,
            "first_task": "Run the failing test",
            "intent": {"kind": "process", "harness": "claude", "cwd": "/work", "options": []}
        });
        let request: crate::api::StartLaunch = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), wire);
        for (field, value) in [
            ("argv", json!(["sh", "-c", "unexpected"])),
            ("env", json!({"TOKEN": "unexpected"})),
            ("first_task", json!("unexpected")),
            ("provider", json!("pulse")),
            ("endpoint", json!("http://unexpected")),
        ] {
            let mut invalid = wire.clone();
            invalid["intent"][field] = value;
            assert!(
                serde_json::from_value::<crate::api::StartLaunch>(invalid).is_err(),
                "accepted process intent field {field}"
            );
        }
    }

    #[test]
    fn backend_process_intent_preserves_execution_fields_and_rejects_task_fields() {
        let wire = json!({
            "version": crate::PROTOCOL_VERSION, "name": "process",
            "launch_id": "launch-0123456789abcdef0123456789abcdef",
            "marshal_version": crate::MARSHAL_VERSION,
            "context": {"daemon_address": "ws://marshal.mesh:6155", "hook_base": null},
            "intent": {"kind": "process", "harness": "claude", "cwd": "/work", "options": [],
                "argv": ["claude", "literal; $(touch forbidden)"], "env": {"FIXTURE_VALUE": "literal value"}}
        });
        let request: LaunchRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        let process = request.process_request().unwrap();
        assert_eq!(process.argv, ["claude", "literal; $(touch forbidden)"]);
        assert_eq!(process.env["FIXTURE_VALUE"], "literal value");
        for (field, value) in [
            ("first_task", json!("unexpected")),
            ("provider", json!("pulse")),
            ("endpoint", json!("http://unexpected")),
        ] {
            let mut invalid = wire.clone();
            invalid["intent"][field] = value;
            assert!(
                serde_json::from_value::<LaunchRequest>(invalid).is_err(),
                "accepted backend process field {field}"
            );
        }
    }

    #[test]
    fn workspace_selectors_reject_unexpected_launch_fields() {
        for provider in ["session", "pulseRoot"] {
            for (field, value) in [
                ("argv", json!(["sh"])),
                ("env", json!({"TOKEN": "secret"})),
                ("path", json!("/unexpected")),
                ("other", json!(true)),
            ] {
                let mut wire = json!({"provider": provider});
                wire[field] = value;
                assert!(
                    serde_json::from_value::<PulseWorkspace>(wire).is_err(),
                    "accepted {provider}.{field}"
                );
            }
        }
    }

    #[test]
    fn pulse_workspace_selectors_match_the_pulse_contract() {
        for wire in [
            json!({"provider": "session"}),
            json!({"provider": "pulseRoot"}),
            json!({"provider": "existingDirectory", "path": "/scoped/work", "displayName": null}),
            json!({"provider": "pulse", "repositories": ["repo"], "identity": {"kind": "name", "name": "work"}}),
            json!({"provider": "pulse", "repositories": ["repo"], "identity": {"kind": "leviTask", "taskId": "lv-1234"}}),
        ] {
            let selector: PulseWorkspace = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(selector).unwrap(), wire);
        }
    }
}
