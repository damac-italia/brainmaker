// SPDX-License-Identifier: GPL-3.0-or-later

//! Version comparison for the self-update check.

/// Reports whether `candidate` is newer than `current`.
///
/// The function compares the dotted numeric core of each version, from left to
/// right. A missing component counts as 0, so `1.2` equals `1.2.0`.
///
/// The function ignores a pre-release suffix or a build suffix, which is the
/// text from the first `-` or `+` onward. Two versions with the same numeric
/// core therefore never trigger an update, whatever their suffixes. That rule
/// is deliberate: it can never start an update loop, and it can never install
/// an older build.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    order(candidate, current) == std::cmp::Ordering::Greater
}

fn order(left: &str, right: &str) -> std::cmp::Ordering {
    let left = numeric_core(left);
    let right = numeric_core(right);
    let length = left.len().max(right.len());

    for index in 0..length {
        let l = left.get(index).copied().unwrap_or(0);
        let r = right.get(index).copied().unwrap_or(0);
        match l.cmp(&r) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// Splits the numeric core into components. A component that is not a number
/// counts as 0.
fn numeric_core(version: &str) -> Vec<u64> {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    core.split('.')
        .map(|part| part.trim().parse::<u64>().unwrap_or(0))
        .collect()
}

/// Rejects a version string that we must not print or compare.
///
/// The string reaches us from the network. We accept digits, dots, hyphens,
/// plus signs, and ASCII letters, up to 64 characters.
pub fn validate(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_newer_version() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.2.3", "1.2"));
    }

    #[test]
    fn rejects_an_equal_or_older_version() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("1.2", "1.2.0"));
    }

    #[test]
    fn ignores_a_prerelease_or_build_suffix() {
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0-beta"));
        assert!(!is_newer("0.1.0+build9", "0.1.0"));
        assert!(is_newer("0.2.0-beta", "0.1.0"));
    }

    #[test]
    fn tolerates_a_leading_v_and_junk_components() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("not-a-version", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
    }

    #[test]
    fn validates_a_version_string() {
        assert!(validate("0.2.0"));
        assert!(validate("1.0.0-rc.1+build9"));
        assert!(!validate(""));
        assert!(!validate("0.2.0; rm -rf /"));
        assert!(!validate("0.2.0\n"));
        assert!(!validate(&"9".repeat(65)));
    }
}
