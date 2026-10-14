// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Where an appliance answers: an IP address or a DNS name, such as a
//! dynamic DNS name for an appliance on a changing address, with an
//! optional port.
//!
//! A name is resolved again on every connection, so a dynamic DNS update is
//! followed without any change in BMAC. The name is never the appliance's
//! identity: every connection checks the certificate fingerprint recorded
//! at enrollment or typed on the Trusted peers page, so whoever controls
//! the DNS record can make connections fail but can't stand in for the
//! appliance.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::InvalidValue;

const KIND: &str = "appliance address";
/// The longest DNS name, without the trailing dot.
const NAME_MAX: usize = 253;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AddressHost {
    Ip(IpAddr),
    /// Lowercase, without a trailing dot.
    Name(String),
}

/// `203.0.113.5`, `[2001:db8::1]:9445`, `backup.example.org`, or
/// `backup.example.dyndns.org:9445`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
pub struct ApplianceAddress {
    pub host: AddressHost,
    pub port: Option<u16>,
}

impl ApplianceAddress {
    pub fn port_or(&self, default: u16) -> u16 {
        self.port.unwrap_or(default)
    }

    /// The host part as a resolver or TLS client takes it: an IP address's
    /// text, or the name.
    pub fn host_text(&self) -> String {
        match &self.host {
            AddressHost::Ip(ip) => ip.to_string(),
            AddressHost::Name(name) => name.clone(),
        }
    }

    pub fn is_name(&self) -> bool {
        matches!(self.host, AddressHost::Name(_))
    }
}

fn dns_name(text: &str) -> Result<String, &'static str> {
    let name = text.strip_suffix('.').unwrap_or(text);
    if name.is_empty() || name.len() > NAME_MAX {
        return Err("a name must be 1 to 253 characters");
    }
    let labels: Vec<&str> = name.split('.').collect();
    for label in &labels {
        if label.is_empty() || label.len() > 63 {
            return Err("each part of a name must be 1 to 63 characters");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("a part of a name must not start or end with a hyphen");
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("a name has only letters, digits, hyphens, and dots");
        }
    }
    if labels
        .last()
        .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("not an IP address, and a name can't end in a number");
    }
    Ok(name.to_ascii_lowercase())
}

fn port(text: &str) -> Result<u16, &'static str> {
    match text.parse::<u16>() {
        Ok(p) if p > 0 && !text.starts_with('0') => Ok(p),
        _ => Err("the port must be 1 to 65535"),
    }
}

fn parse(text: &str) -> Result<ApplianceAddress, &'static str> {
    if text.is_empty() {
        return Err("it is empty");
    }
    if text.contains("://") || text.contains(['/', '@', '?', '#']) {
        return Err("give only a name or IP address and an optional port, not a URL");
    }
    if text
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err("it must not contain spaces");
    }
    if let Some(rest) = text.strip_prefix('[') {
        let (ip, after) = rest
            .split_once(']')
            .ok_or("an IPv6 address needs its closing ]")?;
        let ip: Ipv6Addr = ip.parse().map_err(|_| "not an IPv6 address")?;
        let port = match after {
            "" => None,
            _ => Some(port(
                after
                    .strip_prefix(':')
                    .ok_or("a port follows ] after a colon")?,
            )?),
        };
        return Ok(ApplianceAddress {
            host: AddressHost::Ip(IpAddr::V6(ip)),
            port,
        });
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Ok(ApplianceAddress {
            host: AddressHost::Ip(ip),
            port: None,
        });
    }
    if text.matches(':').count() > 1 {
        return Err("not an IPv6 address; write one with a port as [address]:port");
    }
    let (host, port) = match text.rsplit_once(':') {
        Some((host, p)) => (host, Some(port(p)?)),
        None => (text, None),
    };
    let host = match host.parse::<IpAddr>() {
        Ok(ip) => AddressHost::Ip(ip),
        Err(_) => AddressHost::Name(dns_name(host)?),
    };
    Ok(ApplianceAddress { host, port })
}

impl FromStr for ApplianceAddress {
    type Err = InvalidValue;
    fn from_str(text: &str) -> Result<Self, InvalidValue> {
        parse(text).map_err(|why| InvalidValue::new(KIND, text, why))
    }
}

impl fmt::Display for ApplianceAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.host, self.port) {
            (AddressHost::Ip(IpAddr::V6(ip)), Some(port)) => write!(f, "[{ip}]:{port}"),
            (host, Some(port)) => write!(f, "{}:{port}", host_text(host)),
            (host, None) => f.write_str(&host_text(host)),
        }
    }
}

fn host_text(host: &AddressHost) -> String {
    match host {
        AddressHost::Ip(ip) => ip.to_string(),
        AddressHost::Name(name) => name.clone(),
    }
}

impl TryFrom<String> for ApplianceAddress {
    type Error = InvalidValue;
    fn try_from(text: String) -> Result<Self, InvalidValue> {
        text.parse()
    }
}

impl From<ApplianceAddress> for String {
    fn from(value: ApplianceAddress) -> String {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(text: &str) -> String {
        text.parse::<ApplianceAddress>().unwrap().to_string()
    }

    #[test]
    fn ip_addresses_and_names_with_or_without_a_port() {
        assert_eq!(shown("203.0.113.5"), "203.0.113.5");
        assert_eq!(shown("203.0.113.5:9445"), "203.0.113.5:9445");
        assert_eq!(shown("2001:db8::1"), "2001:db8::1");
        assert_eq!(shown("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(shown("[2001:db8::1]:9445"), "[2001:db8::1]:9445");
        assert_eq!(
            shown("Backup.Example.DynDNS.org."),
            "backup.example.dyndns.org"
        );
        assert_eq!(shown("backup.example.org:9445"), "backup.example.org:9445");
        assert_eq!(shown("bmac-appliance-b"), "bmac-appliance-b");
        assert_eq!(shown("1backup.example.org"), "1backup.example.org");

        let name: ApplianceAddress = "vault.example.org".parse().unwrap();
        assert!(name.is_name());
        assert_eq!(
            (name.host_text(), name.port_or(8445)),
            ("vault.example.org".into(), 8445)
        );
        let ip: ApplianceAddress = "[2001:db8::1]:9445".parse().unwrap();
        assert!(!ip.is_name());
        assert_eq!(
            (ip.host_text(), ip.port_or(8445)),
            ("2001:db8::1".into(), 9445)
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in [
            "",
            "https://backup.example.org",
            "backup.example.org/v1",
            "admin@backup.example.org",
            "backup example.org",
            "-backup.example.org",
            "backup-.example.org",
            "backup..example.org",
            "back_up.example.org",
            "999.1.1.1",
            "10.0.0",
            "backup.example.org:0",
            "backup.example.org:65536",
            "backup.example.org:08445",
            "backup.example.org:",
            "2001:db8::1:9445x",
            "[2001:db8::1",
            "[2001:db8::1]9445",
            "[203.0.113.5]:9445",
            &format!("{}.org", "a".repeat(64)),
            &format!("{}org", "a.".repeat(127)),
        ] {
            assert!(
                bad.parse::<ApplianceAddress>().is_err(),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn it_travels_as_its_text() {
        let address: ApplianceAddress =
            serde_json::from_value(serde_json::json!("backup.example.org:9445")).unwrap();
        assert_eq!(
            serde_json::to_value(&address).unwrap(),
            "backup.example.org:9445"
        );
        assert!(serde_json::from_value::<ApplianceAddress>(serde_json::json!("a b")).is_err());
    }
}
