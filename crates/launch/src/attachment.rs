use agent_attachment_protocol::{AttachmentReference, AttachmentResolution, Observation};
use anyhow::{Context, Result, ensure};

use crate::{AttachmentPlan, BackendAttachment, api};

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

#[derive(Debug, Default)]
pub enum Route {
    #[default]
    Unspecified,
    OwnerLocal,
    Ssh {
        host: String,
        origin: Option<String>,
        program: String,
    },
}

pub fn options(args: &[String]) -> Result<(Option<String>, Route)> {
    let mut daemon = None;
    let mut host = None;
    let mut origin = None;
    let mut program = None;
    let mut local = false;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag == "--owner-local" {
            ensure!(!local, "duplicate --owner-local");
            local = true;
            continue;
        }
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?
            .clone();
        let slot = match flag.as_str() {
            "--daemon" => &mut daemon,
            "--on" => &mut host,
            "--owner-daemon" => &mut origin,
            "--owner-program" => &mut program,
            _ => anyhow::bail!("unknown attachment option {flag}"),
        };
        ensure!(slot.is_none(), "duplicate {flag}");
        *slot = Some(value);
    }
    ensure!(
        !local || host.is_none(),
        "--owner-local and --on are exclusive"
    );
    ensure!(
        host.is_some() || (origin.is_none() && program.is_none()),
        "--owner-daemon and --owner-program require --on"
    );
    let route = if let Some(host) = host {
        ensure!(
            !host.is_empty()
                && !host.starts_with('-')
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._:@%+-[]".contains(c)),
            "invalid owner SSH destination"
        );
        let program = program.unwrap_or_else(|| "marshal-launch".into());
        ensure!(
            program == "marshal-launch" || program.starts_with('/'),
            "--owner-program must be an absolute Unix path"
        );
        Route::Ssh {
            host,
            origin,
            program,
        }
    } else if local {
        Route::OwnerLocal
    } else {
        Route::Unspecified
    };
    Ok((daemon, route))
}

pub fn backend_plan(view: &api::LaunchView) -> Result<&BackendAttachment> {
    ensure!(
        view.process == api::ProcessState::Running,
        "launch is not confirmed running ({:?})",
        view.process
    );
    let attachment = view
        .launched
        .as_ref()
        .and_then(|launched| launched.attachment_plan.as_ref())
        .context("structured attachment is unsupported for this launch")?;
    attachment.plan().command().validate()?;
    Ok(attachment)
}

pub fn routed(
    origin: &str,
    launch_id: &str,
    view: &api::LaunchView,
    route: &Route,
) -> Result<BackendAttachment> {
    let attachment = backend_plan(view)?;
    match (attachment, route) {
        (BackendAttachment::ClientLocal { .. }, _) => Ok(attachment.clone()),
        (BackendAttachment::OwnerLocal { .. }, Route::OwnerLocal) => Ok(attachment.clone()),
        (BackendAttachment::OwnerLocal { .. }, Route::Unspecified) => anyhow::bail!(
            "native attachment requires --on <owner-ssh-host> or explicit --owner-local on the daemon host"
        ),
        (
            BackendAttachment::OwnerLocal { .. },
            Route::Ssh {
                host,
                origin: owner_origin,
                program,
            },
        ) => {
            let owner_origin = owner_origin.as_deref().unwrap_or(origin);
            target(owner_origin, launch_id).validate()?;
            let quote = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
            let remote = [
                program.as_str(),
                "attach",
                launch_id,
                "--daemon",
                owner_origin,
                "--owner-local",
            ]
            .map(quote)
            .join(" ");
            let plan = AttachmentPlan::LocalProcess {
                program: "ssh".into(),
                args: vec!["-t".into(), "--".into(), host.clone(), remote],
                cwd: None,
            };
            plan.command().validate()?;
            Ok(BackendAttachment::OwnerLocal { plan })
        }
    }
}

pub fn resolution(
    origin: &str,
    launch_id: &str,
    view: &api::LaunchView,
    route: &Route,
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
        api::ProcessState::Running => match routed(origin, launch_id, view, route) {
            Ok(_) => Observation::Attachable {
                plan: AttachmentPlan::LocalProcess {
                    program: "marshal-launch".into(),
                    args: {
                        let mut args = vec![
                            "attach".into(),
                            launch_id.into(),
                            "--daemon".into(),
                            origin.into(),
                        ];
                        match route {
                            Route::Unspecified => {}
                            Route::OwnerLocal => args.push("--owner-local".into()),
                            Route::Ssh {
                                host,
                                origin,
                                program,
                            } => {
                                args.extend([
                                    "--on".into(),
                                    host.clone(),
                                    "--owner-program".into(),
                                    program.clone(),
                                ]);
                                if let Some(origin) = origin {
                                    args.extend(["--owner-daemon".into(), origin.clone()]);
                                }
                            }
                        }
                        args
                    },
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

pub fn execute(attachment: &BackendAttachment) -> Result<()> {
    let spec = attachment.plan().command();
    spec.validate()?;
    let mut command = std::process::Command::new(spec.program);
    command.args(spec.args);
    if matches!(attachment, BackendAttachment::OwnerLocal { .. }) {
        for key in crate::external::DAEMON_CREDENTIAL_ENV {
            command.env_remove(key);
        }
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
