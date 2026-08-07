// SPDX-License-Identifier: GPL-3.0-or-later

//! The URL parsing that brainmaker needs, and the base-URL policy.
//!
//! One decision needs the scheme, the host, and the port of a URL: whether a
//! plain-HTTP base URL points at this machine. It reads one parser here, so
//! that the decision rests on a parsed origin rather than on a string prefix.
//! Both base URLs, `BRAINMAKER_API_BASE` and `SWETSI_JWT_ENDPOINT`, take the
//! same rule.

use anyhow::{Result, bail};

/// The scheme, the host, and the port of a URL, in lower case.
///
/// The port is always filled in, so that `https://host` and `https://host:443`
/// compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: String,
    /// The host, without the brackets that surround an IPv6 address in a URL.
    pub host: String,
    pub port: u16,
}

impl Origin {
    /// Reports whether the host names this machine.
    ///
    /// brainmaker accepts a plain-HTTP base URL only for such a host, because
    /// the bearer token then never reaches the network.
    pub fn is_loopback(&self) -> bool {
        if self.host == "localhost" {
            return true;
        }
        match self.host.parse::<std::net::IpAddr>() {
            Ok(address) => address.is_loopback(),
            Err(_) => false,
        }
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}:{}", self.scheme, self.host, self.port)
    }
}

/// Reads the origin of an absolute `http` or `https` URL.
///
/// Returns `None` for any other scheme, for a URL with no host, and for a port
/// that is not a number. The function drops any userinfo, so that
/// `https://good.example@evil.example/` reads as `evil.example`.
pub fn parse(url: &str) -> Option<Origin> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443u16,
        "http" => 80u16,
        _ => return None,
    };

    let authority = rest.split(['/', '?', '#']).next()?;
    // Drop any userinfo, which must not take part in the comparison.
    let authority = authority.rsplit('@').next()?;
    if authority.is_empty() {
        return None;
    }

    let (host, port) = split_host_port(authority, default_port)?;
    if host.is_empty() {
        return None;
    }

    Some(Origin {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Splits an authority into its host and its port.
///
/// The function keeps a bracketed IPv6 address whole, because such an address
/// holds the colons that otherwise separate the port. An unbracketed authority
/// with more than one colon has no host and no port that we can read, so it
/// fails.
fn split_host_port(authority: &str, default_port: u16) -> Option<(&str, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if after.is_empty() {
            return Some((host, default_port));
        }
        let port = after.strip_prefix(':')?;
        return Some((host, port.parse().ok()?));
    }

    match authority.split_once(':') {
        Some((host, port)) => Some((host, port.parse().ok()?)),
        None => Some((authority, default_port)),
    }
}

/// Fails when a base URL must not carry a credential.
///
/// brainmaker sends the client secret to the OAuth2 endpoint, and
/// `Authorization: Bearer <token>` on every API request, so it requires
/// `https://`. It accepts `http://` only when the host is this machine, because
/// the credential then never reaches the network.
pub fn check_base_url(key: &str, value: &str) -> Result<()> {
    let Some(origin) = parse(value) else {
        bail!("{key} must be a URL that starts with a scheme and names a host, got {value:?}");
    };

    if origin.scheme == "http" && !origin.is_loopback() {
        bail!(
            "{key} must use TLS, got {value:?}. brainmaker sends a credential on \
             every request, and a plain connection would send it in clear text. A plain \
             connection is accepted only for localhost."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(url: &str) -> Origin {
        parse(url).unwrap_or_else(|| panic!("expected an origin for {url}"))
    }

    #[test]
    fn fills_in_the_default_port() {
        assert_eq!(origin("https://api.example.test/v1").port, 443);
        assert_eq!(origin("http://localhost/v1").port, 80);
    }

    #[test]
    fn an_explicit_default_port_equals_an_implicit_one() {
        assert_eq!(
            origin("https://api.example.test/v1"),
            origin("https://api.example.test:443/other")
        );
        assert_eq!(
            origin("http://localhost/v1"),
            origin("http://localhost:80/other")
        );
    }

    #[test]
    fn a_different_port_is_a_different_origin() {
        assert_ne!(
            origin("https://api.example.test"),
            origin("https://api.example.test:8443")
        );
    }

    #[test]
    fn lowers_the_scheme_and_the_host() {
        let value = origin("HTTPS://API.EXAMPLE.TEST/Path");
        assert_eq!(value.scheme, "https");
        assert_eq!(value.host, "api.example.test");
    }

    #[test]
    fn drops_the_userinfo() {
        assert_eq!(
            origin("https://api.example.test@evil.example/x").host,
            "evil.example"
        );
        assert_eq!(
            origin("https://user:pass@api.example.test/x").host,
            "api.example.test"
        );
    }

    #[test]
    fn reads_a_bracketed_ipv6_address() {
        assert_eq!(origin("http://[::1]/v1").host, "::1");
        assert_eq!(origin("http://[::1]/v1").port, 80);
        assert_eq!(origin("http://[::1]:8080/v1").port, 8080);
    }

    #[test]
    fn rejects_a_url_we_cannot_read() {
        assert_eq!(parse("not a url"), None);
        assert_eq!(parse("ftp://api.example.test"), None);
        assert_eq!(parse("https://"), None);
        assert_eq!(parse("https://api.example.test:notaport"), None);
        assert_eq!(parse("http://::1/v1"), None);
    }

    #[test]
    fn recognises_a_loopback_host() {
        assert!(origin("http://localhost:8080").is_loopback());
        assert!(origin("http://127.0.0.1:8080").is_loopback());
        assert!(origin("http://127.0.0.53").is_loopback());
        assert!(origin("http://[::1]:8080").is_loopback());
    }

    #[test]
    fn a_host_that_only_looks_like_localhost_is_not_loopback() {
        assert!(!origin("http://localhost.evil.example").is_loopback());
        assert!(!origin("http://notlocalhost").is_loopback());
        assert!(!origin("http://128.0.0.1").is_loopback());
        assert!(!origin("http://api.example.test").is_loopback());
    }

    #[test]
    fn accepts_a_tls_base_url() {
        assert!(check_base_url("KEY", "https://api.example.test/v1").is_ok());
        assert!(check_base_url("KEY", "https://api.example.test:8443/v1").is_ok());
    }

    #[test]
    fn accepts_a_plain_base_url_only_on_this_machine() {
        assert!(check_base_url("KEY", "http://localhost:8080/v1").is_ok());
        assert!(check_base_url("KEY", "http://127.0.0.1:8080/v1").is_ok());
        assert!(check_base_url("KEY", "http://[::1]:8080/v1").is_ok());
    }

    #[test]
    fn rejects_a_plain_base_url_that_leaves_this_machine() {
        let error = check_base_url("KEY", "http://api.example.test/v1").unwrap_err();
        assert!(format!("{error}").contains("clear text"), "got {error}");
        assert!(check_base_url("KEY", "http://localhost.evil.example/v1").is_err());
        // Userinfo must not smuggle a remote host past the loopback rule.
        assert!(check_base_url("KEY", "http://localhost@evil.example/v1").is_err());
    }

    #[test]
    fn rejects_a_base_url_with_no_scheme_or_no_host() {
        assert!(check_base_url("KEY", "api.example.test").is_err());
        assert!(check_base_url("KEY", "ftp://api.example.test").is_err());
        assert!(check_base_url("KEY", "").is_err());
    }
}
