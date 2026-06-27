//! Canonical host classification, shared by the web-fetch SSRF blocklist and
//! the provider `base_url` plaintext-http gate.
//!
//! Both used to hand-roll their own IPv4-centric checks that disagreed on IPv6
//! (one missed IPv4-mapped / ULA / link-local / CGNAT, the other was too strict
//! and refused legitimate ULA local servers). This is the one place host
//! routing class is decided.
//!
//! Classification is purely lexical (no DNS): an unresolved name that isn't
//! `localhost` is treated as [`HostClass::Public`], because a no-DNS check
//! can't see where a name resolves.

use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    /// `127.0.0.0/8`, `::1`, `localhost` / `*.localhost`.
    Loopback,
    /// `169.254.0.0/16` (incl. cloud metadata `169.254.169.254`), `fe80::/10`.
    LinkLocal,
    /// RFC-1918 (`10/8`, `172.16/12`, `192.168/16`) and IPv6 ULA `fc00::/7`.
    Private,
    /// Carrier-grade NAT `100.64.0.0/10` (also some cloud metadata fronts).
    Cgnat,
    /// `0.0.0.0`, `::`, IPv4 broadcast.
    Unspecified,
    /// Routable, or an unresolved DNS name.
    Public,
}

impl HostClass {
    /// True for any non-public host. Used by the web-fetch SSRF blocklist
    /// (block everything that isn't clearly routable).
    pub fn is_internal(self) -> bool {
        !matches!(self, HostClass::Public)
    }

    /// True only for loopback. Used by the provider `base_url` gate: plaintext
    /// `http` is acceptable to loopback (no network exposure), but sending an
    /// API key over `http` to any other host — even a LAN/private one — leaks
    /// it in cleartext.
    pub fn is_loopback(self) -> bool {
        matches!(self, HostClass::Loopback)
    }
}

/// Classify a URL host (hostname or IP literal, with optional `[]` around an
/// IPv6 literal and an optional trailing FQDN dot).
pub fn classify_host(host: &str) -> HostClass {
    let h = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return HostClass::Loopback;
    }
    if let Ok(ip) = h.parse::<Ipv4Addr>() {
        return classify_ipv4(ip);
    }
    if let Ok(ip) = h.parse::<Ipv6Addr>() {
        // IPv4-mapped (`::ffff:a.b.c.d`): classify the embedded address so
        // `[::ffff:127.0.0.1]` / `[::ffff:169.254.169.254]` aren't treated as
        // an opaque (and thus "public") IPv6 literal.
        if let Some(v4) = ip.to_ipv4_mapped() {
            return classify_ipv4(v4);
        }
        if ip.is_loopback() {
            return HostClass::Loopback;
        }
        if ip.is_unspecified() {
            return HostClass::Unspecified;
        }
        if (ip.segments()[0] & 0xfe00) == 0xfc00 {
            return HostClass::Private; // ULA fc00::/7
        }
        if (ip.segments()[0] & 0xffc0) == 0xfe80 {
            return HostClass::LinkLocal; // fe80::/10
        }
        return HostClass::Public;
    }
    HostClass::Public
}

fn classify_ipv4(ip: Ipv4Addr) -> HostClass {
    if ip.is_loopback() {
        return HostClass::Loopback;
    }
    if ip.is_unspecified() || ip.is_broadcast() {
        return HostClass::Unspecified;
    }
    if ip.is_link_local() {
        return HostClass::LinkLocal;
    }
    if ip.is_private() {
        return HostClass::Private;
    }
    let o = ip.octets();
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return HostClass::Cgnat; // 100.64.0.0/10
    }
    HostClass::Public
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_forms() {
        for h in [
            "localhost",
            "localhost.",
            "127.0.0.1",
            "127.1.2.3",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "app.localhost",
        ] {
            assert_eq!(classify_host(h), HostClass::Loopback, "{h}");
            assert!(classify_host(h).is_internal());
            assert!(classify_host(h).is_loopback());
        }
    }

    #[test]
    fn internal_but_not_loopback() {
        // These must be blocked by the SSRF list but NOT exempted from https.
        for h in [
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "[::ffff:169.254.169.254]", // IPv4-mapped link-local (old IPv6 hole)
            "[fc00::1]",                // ULA (old IPv6 hole)
            "[fe80::1]",                // link-local IPv6 (old IPv6 hole)
            "100.100.100.200",          // CGNAT / Alibaba metadata (old IPv4 hole)
            "0.0.0.0",
        ] {
            assert!(classify_host(h).is_internal(), "{h} should be internal");
            assert!(!classify_host(h).is_loopback(), "{h} must not be loopback");
        }
    }

    #[test]
    fn public_hosts() {
        for h in [
            "example.com",
            "8.8.8.8",
            "1.1.1.1",
            "[2606:4700:4700::1111]",
            "api.openai.com",
        ] {
            assert_eq!(classify_host(h), HostClass::Public, "{h}");
            assert!(!classify_host(h).is_internal(), "{h}");
        }
    }
}
