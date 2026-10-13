// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The email templates, embedded from `v3/notify/templates`
//! (`MONITORING.md`, "Email templates"), and the typed model of each.
//! Nothing here edits a template while sending; Settings creates missing
//! ones from these copies.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;

pub const MANIFEST: &str = include_str!("../../../notify/templates/manifest.json");
pub const LAYOUT: &str = "bmac-layout-v1";
pub const PRODUCT_NAME: &str = "BMAC";

macro_rules! files {
    ($($alias:literal),* $(,)?) => {
        &[$((
            $alias,
            include_str!(concat!("../../../notify/templates/", $alias, "/body.txt")),
            include_str!(concat!("../../../notify/templates/", $alias, "/body.html")),
        )),*]
    };
}

const SUBJECTS: &[(&str, &str)] = &[
    (
        "bmac-alert-opened-v1",
        include_str!("../../../notify/templates/bmac-alert-opened-v1/subject.txt"),
    ),
    (
        "bmac-alert-resolved-v1",
        include_str!("../../../notify/templates/bmac-alert-resolved-v1/subject.txt"),
    ),
    (
        "bmac-digest-v1",
        include_str!("../../../notify/templates/bmac-digest-v1/subject.txt"),
    ),
    (
        "bmac-test-v1",
        include_str!("../../../notify/templates/bmac-test-v1/subject.txt"),
    ),
    (
        "bmac-watchdog-down-v1",
        include_str!("../../../notify/templates/bmac-watchdog-down-v1/subject.txt"),
    ),
    (
        "bmac-watchdog-up-v1",
        include_str!("../../../notify/templates/bmac-watchdog-up-v1/subject.txt"),
    ),
];

const BODIES: &[(&str, &str, &str)] = files!(
    "bmac-layout-v1",
    "bmac-alert-opened-v1",
    "bmac-alert-resolved-v1",
    "bmac-digest-v1",
    "bmac-test-v1",
    "bmac-watchdog-down-v1",
    "bmac-watchdog-up-v1",
);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestEntry {
    pub alias: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub state: String,
    #[serde(default)]
    pub layout: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
    pub fields: Map<String, Value>,
    pub test_model: Map<String, Value>,
}

/// One template as Postmark receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub entry: ManifestEntry,
    pub subject: String,
    pub text: String,
    pub html: String,
}

impl Template {
    pub fn alias(&self) -> &str {
        &self.entry.alias
    }

    pub fn is_layout(&self) -> bool {
        self.entry.kind == "Layout"
    }

    /// The Templates API's create body.
    pub fn body(&self) -> Value {
        let mut out = serde_json::json!({
            "Name": self.entry.name,
            "Alias": self.entry.alias,
            "HtmlBody": self.html,
            "TextBody": self.text,
            "TemplateType": self.entry.kind,
        });
        if !self.is_layout() {
            out["Subject"] = Value::from(self.subject.clone());
            out["LayoutTemplate"] = Value::from(self.entry.layout.clone());
        }
        out
    }

    /// Whether a template read back from Postmark has this content.
    pub fn matches(&self, live: &Value) -> bool {
        let text = |key: &str| live[key].as_str().unwrap_or("").replace("\r\n", "\n");
        let same = text("Name") == self.entry.name
            && text("HtmlBody") == self.html
            && text("TextBody") == self.text
            && text("TemplateType") == self.entry.kind;
        if self.is_layout() {
            same && text("Subject").is_empty() && text("LayoutTemplate").is_empty()
        } else {
            same && text("Subject") == self.subject
                && Some(text("LayoutTemplate")) == self.entry.layout
        }
    }

    /// The same hash as `dev/postmark_templates.py`'s `content_hash`.
    pub fn content_hash(&self) -> String {
        let mut digest = <Sha256 as sha2::Digest>::new();
        for part in [&self.subject, &self.text, &self.html] {
            sha2::Digest::update(&mut digest, part.as_bytes());
            sha2::Digest::update(&mut digest, [0]);
        }
        sha2::Digest::finalize(digest)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// The license header is for the repository, not for email.
pub fn strip_header(text: &str) -> &str {
    match text.split_once("\n\n") {
        Some((head, rest)) if head.contains("SPDX-License-Identifier") => rest,
        _ => text,
    }
}

/// Every embedded template, the layout first.
pub fn all() -> Vec<Template> {
    #[derive(Deserialize)]
    struct Manifest {
        templates: Vec<ManifestEntry>,
    }
    let manifest: Manifest =
        serde_json::from_str(MANIFEST).expect("the manifest is checked by tests");
    manifest
        .templates
        .into_iter()
        .map(|entry| {
            let (_, text, html) = BODIES
                .iter()
                .find(|(alias, _, _)| *alias == entry.alias)
                .expect("every manifest template is embedded");
            let subject = SUBJECTS
                .iter()
                .find(|(alias, _)| *alias == entry.alias)
                .map(|(_, s)| strip_header(s).trim().to_string())
                .unwrap_or_default();
            Template {
                subject,
                text: strip_header(text).to_string(),
                html: strip_header(html).to_string(),
                entry,
            }
        })
        .collect()
}

/// The fields every template gets from the layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Layout {
    pub product_name: String,
    pub cluster_name: String,
    pub dashboard_url: String,
    pub sent_at: String,
}

impl Layout {
    pub fn new(cluster_name: &str, dashboard_url: &str, sent_at: &str) -> Layout {
        Layout {
            product_name: PRODUCT_NAME.into(),
            cluster_name: cluster_name.into(),
            dashboard_url: dashboard_url.into(),
            sent_at: sent_at.into(),
        }
    }
}

/// A template's own fields.
pub trait Model: Serialize {
    const ALIAS: &'static str;
}

/// The model Postmark renders: the layout's fields and the template's.
pub fn render<M: Model>(layout: &Layout, model: &M) -> Value {
    let mut out = serde_json::to_value(layout).expect("models serialize");
    let own = serde_json::to_value(model).expect("models serialize");
    if let (Value::Object(out), Value::Object(own)) = (&mut out, own) {
        out.extend(own);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertOpened {
    pub alert_id: String,
    pub severity: String,
    pub summary: String,
    pub host: String,
    pub resource: String,
    pub condition: String,
    pub opened_at: String,
    pub opened_at_local: String,
    pub next_step: String,
    pub alert_url: String,
}

impl Model for AlertOpened {
    const ALIAS: &'static str = "bmac-alert-opened-v1";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertResolved {
    pub alert_id: String,
    pub severity: String,
    pub summary: String,
    pub host: String,
    pub resource: String,
    pub condition: String,
    pub opened_at: String,
    pub resolved_at: String,
    pub resolved_at_local: String,
    pub alert_url: String,
}

impl Model for AlertResolved {
    const ALIAS: &'static str = "bmac-alert-resolved-v1";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestAlert {
    pub severity: String,
    pub host: String,
    pub opened_at: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    pub open_count: u64,
    pub alerts: Vec<DigestAlert>,
    pub backup_summary: String,
    pub alerts_url: String,
}

impl Model for Digest {
    const ALIAS: &'static str = "bmac-digest-v1";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestEmail {
    pub requested_by: String,
    pub settings_url: String,
}

impl Model for TestEmail {
    const ALIAS: &'static str = "bmac-test-v1";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogHost {
    pub host: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogDown {
    pub appliance_name: String,
    pub down_since: String,
    pub hosts: Vec<WatchdogHost>,
}

impl Model for WatchdogDown {
    const ALIAS: &'static str = "bmac-watchdog-down-v1";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsweringHost {
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogUp {
    pub appliance_name: String,
    pub down_since: String,
    pub up_at: String,
    pub hosts: Vec<AnsweringHost>,
}

impl Model for WatchdogUp {
    const ALIAS: &'static str = "bmac-watchdog-up-v1";
}

/// Every alias this version sends with, and the layout they use.
pub const USED: &[&str] = &[
    LAYOUT,
    AlertOpened::ALIAS,
    AlertResolved::ALIAS,
    Digest::ALIAS,
    TestEmail::ALIAS,
    WatchdogDown::ALIAS,
    WatchdogUp::ALIAS,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                Value::Object(map.iter().map(|(k, v)| (k.clone(), shape(v))).collect())
            }
            Value::Array(items) => Value::Array(items.iter().take(1).map(shape).collect()),
            _ => Value::from("scalar"),
        }
    }

    fn manifest_shape(fields: &Map<String, Value>) -> Value {
        Value::Object(
            fields
                .iter()
                .map(|(name, kind)| {
                    let shaped = match kind {
                        Value::Array(items) => Value::Array(
                            items
                                .iter()
                                .take(1)
                                .map(|item| manifest_shape(item.as_object().unwrap()))
                                .collect(),
                        ),
                        _ => Value::from("scalar"),
                    };
                    (name.clone(), shaped)
                })
                .collect(),
        )
    }

    fn entry(alias: &str) -> ManifestEntry {
        all()
            .into_iter()
            .find(|t| t.alias() == alias)
            .unwrap()
            .entry
    }

    /// Each model, deserialized from the manifest's test model, has exactly
    /// the manifest's fields.
    fn check<M: Model + serde::de::DeserializeOwned>() {
        let entry = entry(M::ALIAS);
        let model: M = serde_json::from_value(Value::Object(entry.test_model.clone()))
            .unwrap_or_else(|e| panic!("{}: {e}", M::ALIAS));
        let own = serde_json::to_value(&model).unwrap();
        assert_eq!(shape(&own), manifest_shape(&entry.fields), "{}", M::ALIAS);
        let layout = Layout::new("lab", "https://bmac.example.com/", "now");
        let rendered = render(&layout, &model);
        let mut expected = entry.fields.clone();
        expected.extend(self::entry(LAYOUT).fields);
        assert_eq!(shape(&rendered), manifest_shape(&expected), "{}", M::ALIAS);
    }

    #[test]
    fn every_model_has_exactly_the_manifest_fields() {
        check::<AlertOpened>();
        check::<AlertResolved>();
        check::<Digest>();
        check::<TestEmail>();
        check::<WatchdogDown>();
        check::<WatchdogUp>();
        let layout = serde_json::to_value(Layout::new("a", "b", "c")).unwrap();
        assert_eq!(shape(&layout), manifest_shape(&entry(LAYOUT).fields));
    }

    #[test]
    fn every_manifest_template_is_embedded_without_its_license_header() {
        let templates = all();
        assert_eq!(templates.len(), BODIES.len());
        assert!(templates[0].is_layout(), "the layout comes first");
        for template in &templates {
            for text in [&template.subject, &template.text, &template.html] {
                assert!(!text.contains("SPDX"), "{}", template.alias());
            }
            if template.is_layout() {
                assert!(template.subject.is_empty());
                assert_eq!(template.html.matches("{{{ @content }}}").count(), 1);
            } else {
                assert!(!template.subject.is_empty(), "{}", template.alias());
            }
            if template.entry.state == "released" {
                assert_eq!(
                    template.entry.hash.as_deref(),
                    Some(template.content_hash().as_str()),
                    "{} changed after release",
                    template.alias()
                );
            }
        }
    }

    #[test]
    fn the_aliases_used_are_in_the_manifest_and_released_in_release_builds() {
        let templates = all();
        for alias in USED {
            let template = templates
                .iter()
                .find(|t| t.alias() == *alias)
                .unwrap_or_else(|| panic!("{alias} is not in the manifest"));
            if !cfg!(debug_assertions) {
                assert_eq!(
                    template.entry.state, "released",
                    "a release build sends with {alias}, which is still a draft"
                );
            }
        }
        assert_eq!(USED.len(), templates.len());
    }

    #[test]
    fn a_template_matches_its_own_create_body() {
        for template in all() {
            assert!(template.matches(&template.body()), "{}", template.alias());
            let mut edited = template.body();
            edited["HtmlBody"] = Value::from("edited");
            assert!(!template.matches(&edited));
        }
    }
}
