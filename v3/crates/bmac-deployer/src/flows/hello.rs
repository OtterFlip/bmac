// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Deploying the static hello app to a production guest over the
//! workstation's jump SSH (`crate::hello`). The Cloudflare Origin CA
//! certificate and key are pasted, checked before anything is sent, and
//! held only in memory; the key's field is sensitive, so it is never kept
//! in the task's record.

use std::collections::BTreeMap;

use bmac_core::job::{Field, FieldType, PhaseStatus, RequestForm, Severity};
use serde_json::json;

use super::ssh::{choose_guest, cluster_and_list};
use super::{Env, FieldExt, field, text};
use crate::hello::{self, HelloPlan};
use crate::jump_ssh;
use crate::tasks::{Stop, TaskContext, TaskResult, Values};
use crate::workstation_ssh::valid_name;

fn pem(values: &Values, id: &str) -> Result<Vec<u8>, String> {
    let text = text(values, id).unwrap_or_default();
    if !text.contains("-----BEGIN ") {
        return Err("Paste the whole PEM block, from -----BEGIN to -----END.".into());
    }
    if text.len() > 64 << 10 {
        return Err("That is too long for a certificate or key.".into());
    }
    Ok(text.into_bytes())
}

pub async fn run(ctx: &TaskContext, env: &Env) -> TaskResult {
    let ssh = env.tool(&env.tools.ssh, "ssh")?;

    ctx.phase("guest", "Guest", PhaseStatus::Running);
    let (_, _, list) = cluster_and_list(ctx, env, ssh, "Which cluster's guest?").await?;
    let eligible: Vec<_> = list
        .guests
        .iter()
        .filter(|g| g.status == "running" && HelloPlan::for_guest(g, &list.staging_limits).is_ok())
        .cloned()
        .collect();
    if eligible.is_empty() {
        let reasons: Vec<String> = list
            .guests
            .iter()
            .filter(|g| g.kind == "production")
            .filter_map(|g| HelloPlan::for_guest(g, &list.staging_limits).err())
            .collect();
        return Err(Stop::Failed {
            message: "The cluster has no active production guest the hello app can go to.".into(),
            details: (!reasons.is_empty()).then(|| reasons.join("\n")),
        });
    }
    let guest = choose_guest(
        ctx,
        &eligible,
        "Deploy the hello app to which guest?",
        "Running, active production guests. The app replaces the guest's NGINX site for its domains.",
    )
    .await?;
    let plan = HelloPlan::for_guest(&guest, &list.staging_limits).map_err(Stop::failed)?;
    ctx.phase("guest", "Guest", PhaseStatus::Complete);

    ctx.phase("certificate", "Certificate", PhaseStatus::Running);
    let names = plan.server_names();
    let now_secs = env.now() / 1000;
    let mut description = plan.cloudflare_steps();
    description.push(format!("The certificate must cover {}.", names.join(", ")));
    let values = ctx
        .values(
            RequestForm::InputGroup {
                title: format!("{}'s Cloudflare Origin certificate", plan.primary),
                description: Some(description.join("\n")),
                context: None,
                layout: None,
                fields: vec![
                    field("ssh_name", FieldType::String, "SSH name")
                        .required()
                        .default_value(json!(guest.name))
                        .help("The name `ssh` reaches the guest by; set it up on Workstation SSH."),
                    field("cert", FieldType::Multiline, "Origin certificate (PEM)").required(),
                    Field {
                        sensitive: true,
                        ..field("key", FieldType::Multiline, "Private key (PEM)")
                            .required()
                            .help("Sent only over SSH's standard input, and never saved.")
                    },
                ],
            },
            |values| {
                let mut errors = BTreeMap::new();
                if !valid_name(&text(values, "ssh_name").unwrap_or_default()) {
                    errors.insert("ssh_name".into(), "Use the guest's SSH name.".into());
                }
                let cert = pem(values, "cert")
                    .map_err(|e| errors.insert("cert".into(), e))
                    .ok();
                let key = pem(values, "key")
                    .map_err(|e| errors.insert("key".into(), e))
                    .ok();
                if !errors.is_empty() {
                    return Err((errors, None));
                }
                hello::check_certificate(&cert.unwrap(), &key.unwrap(), &names, now_secs)
                    .map(|_| ())
                    .map_err(|e| (BTreeMap::new(), Some(e)))
            },
        )
        .await?;
    let ssh_name = text(&values, "ssh_name").unwrap_or_default();
    let cert = pem(&values, "cert").map_err(Stop::failed)?;
    let key = pem(&values, "key").map_err(Stop::failed)?;
    let checked = hello::check_certificate(&cert, &key, &names, now_secs).map_err(Stop::failed)?;
    ctx.output(format!(
        "The certificate covers every name and is valid until {}.",
        checked.not_after
    ));
    ctx.phase("certificate", "Certificate", PhaseStatus::Complete);

    jump_ssh::check_jump(&*env.runner, ssh, &ssh_name)
        .await
        .map_err(|_| {
            Stop::failed(format!(
                "ssh {ssh_name} does not work from this workstation. Set up SSH to {} on \
                 Workstation SSH first.",
                guest.name
            ))
        })?;
    let go = ctx
        .confirm(RequestForm::Confirm {
            title: format!("Deploy the hello app to {}?", guest.name),
            message: format!(
                "This replaces the NGINX site for {} on {} and installs the certificate. Nothing \
                 in Proxmox changes.",
                names.join(", "),
                guest.name
            ),
            severity: Severity::Warning,
            confirm_label: Some("Deploy".into()),
            cancel_label: None,
            confirmation_text: None,
            context: None,
            details: Vec::new(),
        })
        .await?;
    if !go {
        return Err(Stop::Cancelled);
    }

    ctx.phase("deploy", "Deploy", PhaseStatus::Running);
    let output = hello::deploy(
        &*env.runner,
        ssh,
        &ssh_name,
        &plan,
        &cert,
        &key,
        now_secs.max(0) as u64,
    )
    .await
    .map_err(Stop::failed)?;
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        ctx.output(line.to_string());
    }
    ctx.phase("deploy", "Deploy", PhaseStatus::Complete);
    ctx.next_step(format!(
        "Open https://{}/ once Cloudflare proxies the domain to the cluster.",
        plan.primary
    ));
    ctx.result(json!({ "guest": guest.name, "url": format!("https://{}/", plan.primary) }));
    Ok(())
}
