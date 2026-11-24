use agent_attachment_protocol::{AttachmentReference, AttachmentResolution, Observation};
use anyhow::{Context, Result, ensure};

use crate::{AttachmentPlan, api};

fn target(origin: &str, launch_id: &str) -> AttachmentReference {
    AttachmentReference::Managed {
        authority: "marshal".into(),
        origin: origin.into(),
        agent_id: launch_id.into(),
    }
}

pub async fn observe(origin: &str, launch_id: &str) -> Result<api::LaunchView> {
    target(origin, launch_id).validate()?;
    let view: api::LaunchView = api::call(
        origin,
        api::GET_LAUNCH_PATH,
        &api::LaunchKey {
            launch_id: launch_id.into(),
        },
    )
    .await?;
    ensure!(
        view.launch_id == launch_id,
        "launch service returned a different launch ID"
    );
    Ok(view)
}

pub fn backend_plan(view: &api::LaunchView) -> Result<&AttachmentPlan> {
    ensure!(
        view.process == api::ProcessState::Running,
        "launch is not confirmed running ({:?})",
        view.process
    );
    let plan = view
        .launched
        .as_ref()
        .and_then(|launched| launched.attachment_plan.as_ref())
        .context("structured attachment is unsupported for this launch")?;
    plan.command().validate()?;
    Ok(plan)
}

pub fn resolution(
    origin: &str,
    launch_id: &str,
    view: &api::LaunchView,
) -> Result<AttachmentResolution> {
    let target = target(origin, launch_id);
    target.validate()?;
    ensure!(
        view.launch_id == launch_id,
        "launch service returned a different launch ID"
    );
    let observation = match view.process {
        api::ProcessState::Unknown => Observation::Uncertain {
            reason: "backend status is unknown".into(),
        },
        api::ProcessState::Running => match backend_plan(view) {
            Ok(_) => Observation::Attachable {
                plan: AttachmentPlan::LocalProcess {
                    program: "marshal-launch".into(),
                    args: vec![
                        "attach".into(),
                        launch_id.into(),
                        "--daemon".into(),
                        origin.into(),
                    ],
                    cwd: None,
                },
            },
            Err(error) => Observation::Unsupported {
                reason: error.to_string(),
            },
        },
        _ => Observation::Unavailable {
            reason: format!("launch is {:?}", view.process),
        },
    };
    let resolution = AttachmentResolution {
        version: 1,
        target,
        observation,
    };
    resolution.validate_for(&resolution.target)?;
    Ok(resolution)
}

pub fn execute(plan: &AttachmentPlan) -> Result<()> {
    let spec = plan.command();
    spec.validate()?;
    let mut command = std::process::Command::new(spec.program);
    command.args(spec.args);
    for key in crate::external::DAEMON_CREDENTIAL_ENV {
        command.env_remove(key);
    }
    if let Some(cwd) = spec.cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("executing attachment")
    }
    #[cfg(not(unix))]
    {
        ensure!(
            command.status().context("executing attachment")?.success(),
            "attachment command failed"
        );
        Ok(())
    }
}
