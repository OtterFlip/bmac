// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Deploying the static hello app to a production guest (planning decision
//! 1), after v2's `deploy_hello_app_to_prod.sh`. It changes nothing in
//! Proxmox: it runs over the workstation's jump SSH to the guest. The NGINX
//! site names the guest's domains and every default staging name, so
//! staging clones inherit a working configuration.
//!
//! The Cloudflare Origin CA certificate and key are checked here (they
//! parse, match, are current, and cover every name), then travel with the
//! other files as one tar on SSH's standard input with a `SHA256SUMS` the
//! guest checks before installing; the key is never on a command line or
//! in a temporary file on the workstation.

use std::path::Path;
use std::time::Duration;

use bmac_exec::{Command, CommandRunner};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use sha2::{Digest, Sha256};

use crate::hostiso::tar::TarWriter;
use crate::jump_ssh::Guest;

const REMOTE_INSTALL: &str = include_str!("remote-install.sh");

/// What the deployment covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloPlan {
    pub guest: String,
    pub primary: String,
    pub aliases: Vec<String>,
    /// The highest staging number any placement host allows.
    pub max_stage: u32,
}

fn valid_domain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').count() >= 2
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

impl HelloPlan {
    /// The plan for an active production guest, with the staging limits
    /// from `bmac guests --json`.
    pub fn for_guest(
        guest: &Guest,
        staging_limits: &std::collections::BTreeMap<String, u32>,
    ) -> Result<HelloPlan, String> {
        if guest.kind != "production" {
            return Err(format!("{} is not a production guest.", guest.name));
        }
        if guest.state != "active" {
            return Err(format!(
                "{} is in registry state '{}'; it must be active.",
                guest.name, guest.state
            ));
        }
        if guest.placement.len() < 2 {
            return Err(format!(
                "{} has fewer than two placement hosts.",
                guest.name
            ));
        }
        for name in std::iter::once(&guest.primary_domain).chain(&guest.aliases) {
            if !valid_domain(name) {
                return Err(format!("{} has an unusable domain: {name:?}", guest.name));
            }
        }
        let mut max_stage = 0;
        for host in &guest.placement {
            let limit = staging_limits
                .get(host)
                .ok_or_else(|| format!("{host}'s staging limit is not set."))?;
            max_stage = max_stage.max(*limit);
        }
        if max_stage == 0 {
            return Err("No placement host allows a staging VM.".into());
        }
        Ok(HelloPlan {
            guest: guest.name.clone(),
            primary: guest.primary_domain.clone(),
            aliases: guest.aliases.clone(),
            max_stage,
        })
    }

    /// The exact names NGINX accepts: the primary, the aliases, and
    /// `stageN<guest>.<primary>` for every possible N.
    pub fn server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let candidates = std::iter::once(self.primary.clone())
            .chain(self.aliases.iter().cloned())
            .chain(
                (1..=self.max_stage).map(|n| format!("stage{n}{}.{}", self.guest, self.primary)),
            );
        for name in candidates {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// Aliases the `*.<primary>` wildcard doesn't cover, which the
    /// certificate must name as well.
    pub fn certificate_extra_names(&self) -> Vec<String> {
        let suffix = format!(".{}", self.primary);
        self.aliases
            .iter()
            .filter(|alias| {
                *alias != &self.primary
                    && !alias
                        .strip_suffix(&suffix)
                        .is_some_and(|label| !label.is_empty() && !label.contains('.'))
            })
            .cloned()
            .collect()
    }

    /// The steps for making the certificate in Cloudflare.
    pub fn cloudflare_steps(&self) -> Vec<String> {
        let mut names = format!("{} and *.{}", self.primary, self.primary);
        let extra = self.certificate_extra_names();
        if !extra.is_empty() {
            names.push_str(&format!(", plus {}", extra.join(", ")));
        }
        vec![
            format!(
                "In the Cloudflare dashboard for the zone containing {}, open SSL/TLS, then \
                 Origin Server, and choose Create Certificate under Origin Certificates.",
                self.primary
            ),
            format!(
                "Choose \"Generate private key and CSR with Cloudflare\" (ECC or RSA). The \
                 certificate's hostnames must include {names}."
            ),
            "Choose the validity period, create the certificate, and choose PEM format.".into(),
            "Save both the Origin Certificate and the Private Key on this workstation as \
             permanent backup files, the key readable only by you. Cloudflare can't show the key \
             again."
                .into(),
        ]
    }

    pub fn site_config(&self) -> String {
        let names: String = self
            .server_names()
            .iter()
            .map(|n| format!("        {n}\n"))
            .collect();
        format!(
            "# Managed by the BMAC Deployer's hello app for {guest}.\n\
             # Production aliases and every default staging name are listed\n\
             # explicitly so staging clones inherit a ready-to-use configuration.\n\
             \n\
             server {{\n    listen 80;\n    listen [::]:80;\n\n    server_name\n{names}        ;\n\n    \
             location = /healthz {{\n        access_log off;\n        default_type text/plain;\n        \
             return 200 \"ok\\n\";\n    }}\n\n    location / {{\n        return 301 https://$host$request_uri;\n    }}\n}}\n\
             \n\
             server {{\n    listen 443 ssl;\n    listen [::]:443 ssl;\n\n    server_name\n{names}        ;\n\n    \
             ssl_certificate     /etc/nginx/ssl/cloudflare-origin.pem;\n    \
             ssl_certificate_key /etc/nginx/ssl/cloudflare-origin.key;\n    \
             ssl_protocols TLSv1.2 TLSv1.3;\n\n    root /var/www/hello-app;\n    index index.html;\n\n    \
             location = /healthz {{\n        access_log off;\n        default_type text/plain;\n        \
             return 200 \"ok\\n\";\n    }}\n\n    location / {{\n        try_files $uri $uri/ =404;\n    }}\n}}\n",
            guest = self.guest
        )
    }

    pub fn index_html(&self) -> String {
        format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\">\n  \
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n  \
             <title>Hello</title>\n</head>\n<body>\n  <h1>Hello, world!</h1>\n  \
             <p>Deployed to {}.</p>\n</body>\n</html>\n",
            self.guest
        )
    }
}

/// What the certificate check found, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedCertificate {
    pub not_after: String,
}

/// The certificate and key parse, belong together, are valid at `now_secs`,
/// and cover every name.
pub fn check_certificate(
    cert_pem: &[u8],
    key_pem: &[u8],
    names: &[String],
    now_secs: i64,
) -> Result<CheckedCertificate, String> {
    let cert = CertificateDer::from_pem_slice(cert_pem)
        .map_err(|_| "The certificate file is not a PEM X.509 certificate.".to_string())?;
    let (_, parsed) = x509_parser::parse_x509_certificate(&cert)
        .map_err(|_| "The certificate file is not a readable X.509 certificate.".to_string())?;
    let validity = parsed.validity();
    if now_secs < validity.not_before.timestamp() || now_secs > validity.not_after.timestamp() {
        return Err("The certificate is expired or not yet valid.".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|_| "The key file is not an unencrypted PEM private key.".to_string())?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|_| "The private key's type isn't supported (use ECC or RSA).".to_string())?;
    let public = signing
        .public_key()
        .ok_or_else(|| "The private key's public half can't be derived.".to_string())?;
    if public.as_ref() != parsed.public_key().raw {
        return Err("The certificate and private key don't match.".into());
    }
    let end_entity = webpki::EndEntityCert::try_from(&cert)
        .map_err(|e| format!("The certificate can't be used for TLS: {e:?}"))?;
    for name in names {
        let server = ServerName::try_from(name.as_str())
            .map_err(|_| format!("{name} is not a host name."))?;
        end_entity
            .verify_is_valid_for_subject_name(&server)
            .map_err(|_| format!("The certificate does not cover {name}."))?;
    }
    Ok(CheckedCertificate {
        not_after: validity.not_after.to_string(),
    })
}

/// The tar sent to the guest, with `SHA256SUMS` first.
pub fn bundle(plan: &HelloPlan, cert_pem: &[u8], key_pem: &[u8], mtime: u64) -> Vec<u8> {
    let names = plan.server_names().join("\n") + "\n";
    let site = plan.site_config();
    let index = plan.index_html();
    let files: [(&str, &[u8], u32); 6] = [
        ("cloudflare-origin.pem", cert_pem, 0o644),
        ("cloudflare-origin.key", key_pem, 0o600),
        ("hello-app.nginx", site.as_bytes(), 0o644),
        ("index.html", index.as_bytes(), 0o644),
        ("server-names", names.as_bytes(), 0o644),
        ("remote-install.sh", REMOTE_INSTALL.as_bytes(), 0o700),
    ];
    pack(&files, mtime)
}

/// A tar of `files` preceded by their `SHA256SUMS`.
pub(crate) fn pack(files: &[(&str, &[u8], u32)], mtime: u64) -> Vec<u8> {
    let sums: String = files
        .iter()
        .map(|(name, bytes, _)| {
            let digest: String = Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{digest}  {name}\n")
        })
        .collect();
    let mut tar = TarWriter::new(mtime);
    tar.file("SHA256SUMS", 0o644, sums.as_bytes());
    for (name, bytes, mode) in files {
        tar.file(name, *mode, bytes);
    }
    tar.finish()
}

/// The fixed command run on the guest; the domain is its only argument and
/// is a validated host name.
pub fn remote_command(primary: &str) -> Result<String, String> {
    if !valid_domain(primary) {
        return Err(format!("{primary:?} is not a domain."));
    }
    Ok(format!(
        "bash -c 'set -Eeuo pipefail; umask 077; d=$(mktemp -d /tmp/hello-app-deploy.XXXXXX); \
         trap \"rm -rf -- \\\"$d\\\"\" EXIT; tar -x -C \"$d\" -f -; cd \"$d\"; \
         sha256sum -c --quiet --strict SHA256SUMS; bash \"$d/remote-install.sh\" \"$d\" \"$1\"' \
         hello-app {primary}"
    ))
}

/// Uploads and installs; the guest checks the sums before anything else.
pub async fn deploy(
    runner: &dyn CommandRunner,
    ssh: &Path,
    ssh_name: &str,
    plan: &HelloPlan,
    cert_pem: &[u8],
    key_pem: &[u8],
    now_secs: u64,
) -> Result<String, String> {
    let payload = bundle(plan, cert_pem, key_pem, now_secs);
    let output = runner
        .run(
            Command::new(ssh)
                .inherit_env("HOME")
                .inherit_env("USER")
                .inherit_env("SSH_AUTH_SOCK")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15", ssh_name])
                .arg(remote_command(&plan.primary)?)
                .stdin(payload)
                .timeout(Duration::from_secs(900)),
        )
        .await
        .map_err(|e| format!("Deploying to {}: {e}", plan.guest))?;
    Ok(output.stdout_text())
}

#[cfg(test)]
pub(crate) mod tests;
