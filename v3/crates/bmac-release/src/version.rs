// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Version order, as dpkg sorts it (`deb-version(7)`), since dpkg decides
//! what an install is: an upgrade, a reinstall, or a downgrade.

use std::cmp::Ordering;

/// A BMAC version as its package's version: `3.0.0-dev.0` becomes
/// `3.0.0~dev.0`, which dpkg sorts before `3.0.0`; build metadata such as
/// `+b2` stays and sorts after the version without it.
pub fn debian_version(version: &str) -> String {
    version.replacen('-', "~", 1)
}

fn order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_alphabetic() => i32::from(c),
        Some(c) => i32::from(c) + 256,
    }
}

/// The digits at `k` without leading zeros, and where they end.
fn digits(s: &[u8], mut k: usize) -> (&[u8], usize) {
    while s.get(k) == Some(&b'0') {
        k += 1;
    }
    let start = k;
    while s.get(k).is_some_and(u8::is_ascii_digit) {
        k += 1;
    }
    (&s[start..k], k)
}

fn compare_part(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        loop {
            let ca = a.get(i).copied().filter(|c| !c.is_ascii_digit());
            let cb = b.get(j).copied().filter(|c| !c.is_ascii_digit());
            if ca.is_none() && cb.is_none() {
                break;
            }
            let (oa, ob) = (order(ca), order(cb));
            if oa != ob {
                return oa.cmp(&ob);
            }
            if ca.is_some() {
                i += 1;
            }
            if cb.is_some() {
                j += 1;
            }
        }
        let (na, ni) = digits(a, i);
        let (nb, nj) = digits(b, j);
        let by_value = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
        if by_value != Ordering::Equal {
            return by_value;
        }
        (i, j) = (ni, nj);
    }
    Ordering::Equal
}

fn split(version: &str) -> (u64, &str, &str) {
    let (epoch, rest) = match version.split_once(':') {
        Some((e, r)) => (e.parse().unwrap_or(0), r),
        None => (0, version),
    };
    match rest.rsplit_once('-') {
        Some((upstream, revision)) => (epoch, upstream, revision),
        None => (epoch, rest, ""),
    }
}

/// Compares two Debian versions as dpkg does.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (ea, ua, ra) = split(a);
    let (eb, ub, rb) = split(b);
    ea.cmp(&eb)
        .then_with(|| compare_part(ua.as_bytes(), ub.as_bytes()))
        .then_with(|| compare_part(ra.as_bytes(), rb.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_as_dpkg_sorts_them() {
        let sorted = [
            "3.0.0~dev.0",
            "3.0.0~dev.0+b1",
            "3.0.0~dev.0+b2",
            "3.0.0~dev.0+b10",
            "3.0.0~dev.1",
            "3.0.0~rc.1",
            "3.0.0",
            "3.0.0+b1",
            "3.0.1",
            "3.1.0",
            "1:0.1",
        ];
        for pair in sorted.windows(2) {
            assert_eq!(
                compare_versions(pair[0], pair[1]),
                Ordering::Less,
                "{pair:?}"
            );
            assert_eq!(compare_versions(pair[1], pair[0]), Ordering::Greater);
        }
        assert_eq!(compare_versions("3.0.0", "3.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.01", "1.1"), Ordering::Equal);
        assert_eq!(compare_versions("1.0-1", "1.0-2"), Ordering::Less);
    }

    #[test]
    fn bmac_versions_become_package_versions() {
        assert_eq!(debian_version("3.0.0-dev.0"), "3.0.0~dev.0");
        assert_eq!(debian_version("3.0.0-dev.0+b2"), "3.0.0~dev.0+b2");
        assert_eq!(debian_version("3.0.1"), "3.0.1");
    }
}
