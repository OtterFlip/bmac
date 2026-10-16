// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The public keys bundles are checked against. The private release key is
//! kept offline; the development key stays on the developers' machines and
//! is trusted only by builds with the `dev-signing` feature, so a
//! production cluster refuses a development bundle.

/// A public Ed25519 key a bundle's signature may check against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedKey {
    /// `release` or `development`, as the Dashboard shows it.
    pub name: &'static str,
    pub public: [u8; 32],
}

const fn hex32(text: &str) -> [u8; 32] {
    let bytes = text.as_bytes();
    assert!(bytes.len() == 64, "a key is 64 hex digits");
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nibble(bytes[2 * i]) << 4) | nibble(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("lowercase hex only"),
    }
}

/// `bmac-release-signing.pub`.
pub const RELEASE_KEY: TrustedKey = TrustedKey {
    name: "release",
    public: hex32("34561142193a5f269fefe3ecbf71e4d40738bc1b37c143a0831924afd49c3f7c"),
};

/// `bmac-dev-signing.pub`.
pub const DEV_KEY: TrustedKey = TrustedKey {
    name: "development",
    public: hex32("b832ab255bb1b10d0e2556b56b0ff62bda1f277ff840634f85fe93d4b0503d36"),
};

/// The keys this build trusts.
pub fn trusted_keys() -> Vec<TrustedKey> {
    let mut keys = vec![RELEASE_KEY];
    if cfg!(feature = "dev-signing") {
        keys.push(DEV_KEY);
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_build_trusts_the_release_key_alone() {
        let names: Vec<&str> = trusted_keys().iter().map(|k| k.name).collect();
        if cfg!(feature = "dev-signing") {
            assert_eq!(names, ["release", "development"]);
        } else {
            assert_eq!(names, ["release"]);
        }
        assert_ne!(RELEASE_KEY.public, DEV_KEY.public);
        assert_eq!(RELEASE_KEY.public[..2], [0x34, 0x56]);
        assert_eq!(DEV_KEY.public[31], 0x36);
    }
}
