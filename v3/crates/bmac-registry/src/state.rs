// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! State sets and transitions (`cluster_registry.py` 45–202).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Production,
    Staging,
}

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }

            pub fn from_text(text: &str) -> Option<$name> {
                match text {
                    $($text => Some($name::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(ResourceState {
    Reserved = "reserved",
    Provisioning = "provisioning",
    Snapshotting = "snapshotting",
    Replicating = "replicating",
    Cloning = "cloning",
    Patching = "patching",
    Stopped = "stopped",
    Ready = "ready",
    Active = "active",
    Stopping = "stopping",
    CleanupPending = "cleanup_pending",
    Failed = "failed",
});

impl ResourceState {
    /// `STATE_TRANSITIONS`: where a guest of `kind` may go from here.
    pub fn next(self, kind: Kind) -> &'static [ResourceState] {
        use ResourceState::*;
        match (kind, self) {
            (Kind::Production, Reserved) => &[Provisioning, Failed, CleanupPending],
            (Kind::Production, Provisioning) => &[Stopped, Replicating, Failed, CleanupPending],
            (Kind::Production, Replicating) => &[Stopped, Ready, Failed, CleanupPending],
            (Kind::Production, Stopped) => &[
                Provisioning,
                Replicating,
                Ready,
                Active,
                Failed,
                CleanupPending,
            ],
            (Kind::Production, Ready) => &[Active, Stopped, Failed, CleanupPending],
            (Kind::Production, Active) => &[Stopping, Stopped, Failed],
            (Kind::Production, Stopping) => &[Stopped, Failed, CleanupPending],
            (Kind::Production, Failed) => &[Provisioning, Stopped, CleanupPending],
            (Kind::Production, CleanupPending) => &[Failed],
            (Kind::Production, Snapshotting | Cloning | Patching) => &[],
            (Kind::Staging, Reserved) => &[Snapshotting, Cloning, Failed, CleanupPending],
            (Kind::Staging, Snapshotting) => &[Replicating, Cloning, Failed, CleanupPending],
            (Kind::Staging, Replicating) => &[Cloning, Failed, CleanupPending],
            (Kind::Staging, Cloning) => &[Patching, Failed, CleanupPending],
            (Kind::Staging, Patching) => &[Stopped, Ready, Failed, CleanupPending],
            (Kind::Staging, Stopped) => &[Ready, Active, Failed, CleanupPending],
            (Kind::Staging, Ready) => &[Active, Stopped, Failed, CleanupPending],
            (Kind::Staging, Active) => &[Stopping, Stopped, Failed],
            (Kind::Staging, Stopping) => &[Stopped, Failed, CleanupPending],
            (Kind::Staging, Failed) => &[Snapshotting, Cloning, Patching, Stopped, CleanupPending],
            (Kind::Staging, CleanupPending) => &[Failed],
            (Kind::Staging, Provisioning) => &[],
        }
    }

    /// `ROUTABLE_STATES`: routes may be enabled.
    pub fn routable(self) -> bool {
        matches!(
            self,
            ResourceState::Stopped | ResourceState::Ready | ResourceState::Active
        )
    }

    /// `RELEASE_STATES`: the allocation may be released.
    pub fn releasable(self) -> bool {
        use ResourceState::*;
        matches!(self, Reserved | Stopped | Failed | CleanupPending)
    }
}

/// The `state_transitions` object every policy carries, as v2 builds it:
/// sorted states, each with its sorted destinations, only states that have
/// an entry for the kind.
pub fn state_transitions_json() -> Value {
    let mut kinds = Map::new();
    for (kind, name) in [(Kind::Production, "production"), (Kind::Staging, "staging")] {
        let mut states: Vec<(&str, Vec<&str>)> = ResourceState::ALL
            .iter()
            .filter(|s| !s.next(kind).is_empty())
            .map(|s| {
                let mut next: Vec<&str> = s.next(kind).iter().map(|n| n.as_str()).collect();
                next.sort_unstable();
                (s.as_str(), next)
            })
            .collect();
        states.sort_unstable();
        let map = states
            .into_iter()
            .map(|(s, n)| (s.to_string(), n.into()))
            .collect();
        kinds.insert(name.into(), Value::Object(map));
    }
    Value::Object(kinds)
}

text_enum!(InstallPhase {
    Unknown = "unknown",
    Unstarted = "unstarted",
    MediaAttached = "media-attached",
    InstallerStarted = "installer-started",
    InstalledConfirmed = "installed-confirmed",
    GuestVerified = "guest-verified",
    NetworkFinalized = "network-finalized",
});

impl InstallPhase {
    /// `INSTALL_PHASE_TRANSITIONS`.
    pub fn next(self) -> &'static [InstallPhase] {
        use InstallPhase::*;
        match self {
            Unknown | Unstarted => &[MediaAttached, InstalledConfirmed],
            MediaAttached => &[InstallerStarted],
            InstallerStarted => &[MediaAttached, InstalledConfirmed],
            InstalledConfirmed => &[GuestVerified],
            GuestVerified => &[NetworkFinalized],
            NetworkFinalized => &[],
        }
    }
}

text_enum!(InstallMode { UbuntuAutoinstall = "ubuntu-autoinstall", Manual = "manual" });

text_enum!(HostSlotState { Joining = "joining", Member = "member" });

text_enum!(IngressNodeState {
    Pending = "pending",
    Staged = "staged",
    Applied = "applied",
    RejectOnly = "reject-only",
});

text_enum!(CleanupAction {
    DeleteSnapshot = "delete-snapshot",
    DestroyVm = "destroy-vm",
    DestroyVolume = "destroy-volume",
    RemoveReplication = "remove-replication",
    RemoveRoute = "remove-route",
});

text_enum!(CleanupState { Pending = "pending", Completed = "completed" });
