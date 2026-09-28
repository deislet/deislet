//! `deis undeploy --target cloudflare` - take an environment's deployment on
//! a provider down.
//!
//! The control plane records an `undeploy` operation that runs the provider
//! driver's `delete` against the environment's latest provider deployment on
//! the target, and this command follows it until it settles. The Worker is
//! removed; the environment's provider data (KV namespaces, databases,
//! buckets, queues) is kept unless `--purge-data <environment>` repeats the
//! environment's name, which is the consent the driver needs to destroy it.

use anyhow::{Result, bail};
use colored::Colorize;
use deis_wire::CanonicalId;

use crate::commands::Session;
use crate::commands::deploy::{DeployTarget, choose_provider_target};
use crate::render::Output;
use crate::settings::Overrides;

pub struct Args {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub target: DeployTarget,
    /// Which provider target, by name or id.
    pub provider_target: Option<String>,
    /// The environment name, repeated as consent to destroy its data.
    pub purge_data: Option<String>,
}

pub async fn execute(args: Args, output: Output) -> Result<()> {
    if args.target == DeployTarget::Deislet {
        bail!(
            "`deis undeploy` takes down a deployment on a provider; pass --target cloudflare. A \
             Deislet environment keeps running whatever version it points at"
        );
    }
    let session = Session::open(&Overrides {
        app: args.app.clone(),
        environment: args.environment.clone(),
        binding: None,
    })?;
    let app = session.app()?.to_string();
    let environment = session.environment()?.to_string();
    if let Some(named) = args.purge_data.as_deref()
        && named != environment
    {
        bail!(
            "--purge-data must repeat the name of the environment being undeployed ({environment}), \
             not {named:?}: it is the consent to destroy that environment's data, and nothing else's"
        );
    }

    // A target the environment still holds a grant on is found by its name
    // or kind; with no grant left (it may have been revoked), an id is taken
    // as given and no target at all lets the control plane pick the one this
    // environment has deployed to.
    let provider_target_id = match args.provider_target.as_deref() {
        Some(requested) => match requested.parse::<CanonicalId>() {
            Ok(id) => Some(id),
            Err(_) => Some(
                choose_provider_target(
                    &session
                        .control
                        .list_provider_target_grants(&app, &environment)
                        .await?,
                    args.target,
                    Some(requested),
                    &app,
                    &environment,
                )?
                .provider_target_id,
            ),
        },
        None => {
            let grants = session
                .control
                .list_provider_target_grants(&app, &environment)
                .await?;
            let of_kind = grants
                .iter()
                .filter(|grant| grant.backend_id == args.target.backend_id())
                .count();
            if of_kind == 0 {
                None
            } else {
                Some(
                    choose_provider_target(&grants, args.target, None, &app, &environment)?
                        .provider_target_id,
                )
            }
        }
    };

    output.line(format!(
        "\n{} {} {} {} {} {}",
        "Undeploying".bold(),
        app.cyan(),
        "from".bold(),
        environment.cyan(),
        "on".bold(),
        args.target.build_target().cyan()
    ));
    if args.purge_data.is_some() {
        output.line(format!(
            "  {} the provider data of {} is destroyed with it",
            "!".yellow(),
            environment.cyan()
        ));
    }
    let accepted = session
        .control
        .undeploy_provider_deployment(
            &app,
            &environment,
            provider_target_id,
            args.purge_data.as_deref(),
        )
        .await?;
    output.line(format!(
        "  {} undeploy {} started",
        "✓".green(),
        accepted.operation_id.to_string().cyan()
    ));
    let settled = session
        .control
        .wait_for_provider_deployment(&app, &environment, accepted.operation_id)
        .await?;
    let succeeded = settled.operation.state == deis_wire::OperationState::Succeeded.as_str();
    if output.json {
        output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "target": args.target.build_target(),
            "purgeData": args.purge_data.is_some(),
            "providerDeployment": settled,
        }))?;
    } else if succeeded {
        println!(
            "\n  {} {} of {} is no longer deployed on {}{} ({})",
            "✓".green(),
            environment.cyan(),
            app.cyan(),
            args.target.build_target(),
            if args.purge_data.is_some() {
                "; its provider data was destroyed"
            } else {
                "; its provider data was kept"
            },
            accepted.operation_id
        );
    }
    if !succeeded {
        let reason = settled
            .operation
            .error
            .as_ref()
            .map(|error| format!(": {} {}", error.code, error.message))
            .unwrap_or_default();
        bail!(
            "undeploy {} ended {}{reason}; `deis provider-deployments get {}` shows the step",
            accepted.operation_id,
            settled.operation.state,
            accepted.operation_id
        );
    }
    Ok(())
}
