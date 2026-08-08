//! Canonical host classification, shared by the web-fetch SSRF blocklist and
//! the provider `base_url` plaintext-http gate.
//!
//! Both used to hand-roll their own IPv4-centric checks that disagreed on IPv6
//! (one missed IPv4-mapped / ULA / link-local / CGNAT, the other was too strict
//! and refused legitimate ULA local servers). This is the one place host
//! routing class is decided.
//!
//! Classification is purely lexical (no DNS): `localhost` is classified as
//! loopback, while any other unresolved name is treated as [`HostClass::Public`]
//! because a no-DNS check can't see where a name resolves.

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
    /// Unspecified, documentation, benchmarking, multicast, transition, and
    /// otherwise reserved/special-purpose address space.
    Unspecified,
    /// Routable, or an unresolved DNS name.
    Public,
}

impl HostClass {
    /// True for any non-public host. Used by the web-fetch SSRF blocklist
    /// (block everything that isn't clearly routable).
    #[must_use]
    pub fn is_internal(self) -> bool {
        !matches!(self, Self::Public)
    }

    /// True only for loopback. Used by the provider `base_url` gate: plaintext
    /// `http` is acceptable to loopback (no network exposure), but sending an
    /// API key over `http` to any other host — even a LAN/private one — leaks
    /// it in cleartext.
    #[must_use]
    pub fn is_loopback(self) -> bool {
        matches!(self, Self::Loopback)
    }
}

/// Classify a URL host (hostname or IP literal, with optional `[]` around an
/// IPv6 literal and an optional trailing FQDN dot).
#[must_use]
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
        return if is_global_ipv6(ip) {
            HostClass::Public
        } else {
            HostClass::Unspecified
        };
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
    if is_global_ipv4(ip) {
        HostClass::Public
    } else {
        HostClass::Unspecified
    }
}

fn is_global_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();

    // RFC 7723 and RFC 8155 anycast services are the two globally reachable
    // exceptions inside the IETF protocol-assignment block.
    if [a, b, c, d] == [192, 0, 0, 9] || [a, b, c, d] == [192, 0, 0, 10] {
        return true;
    }

    !(a == 0 // "this network" 0.0.0.0/8
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // TEST-NET-1
        || (a == 192 && b == 88 && c == 99) // deprecated 6to4 relay anycast
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || a >= 224) // multicast and reserved 224.0.0.0/4 + 240.0.0.0/4
}

fn is_global_ipv6(ip: Ipv6Addr) -> bool {
    let value = u128::from(ip);
    let globally_reachable_protocol_assignment = ip == Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 1)
        || ip == Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 2)
        || ip == Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 3)
        || in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x2001, 3, 0, 0, 0, 0, 0, 0)),
            32,
        )
        || in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x2001, 4, 0x0112, 0, 0, 0, 0, 0)),
            48,
        )
        || in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0)),
            28,
        )
        || in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x2001, 0x30, 0, 0, 0, 0, 0, 0)),
            28,
        );

    // Public IPv6 unicast allocations currently live in 2000::/3. Reject
    // transition/local-use prefixes outside it (for example NAT64), as well as
    // special-purpose sub-ranges inside it. The 2001::/23 protocol block is
    // denied except for the assignments IANA explicitly marks globally
    // reachable; its other tunnelling and benchmarking mechanisms can have an
    // effective endpoint different from the literal address being authorized.
    globally_reachable_protocol_assignment
        || (in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)),
            3,
        ) && !in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0)),
            23,
        ) && !in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0)),
            32,
        ) && !in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0)),
            16,
        ) && !in_ipv6_prefix(
            value,
            u128::from(Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0)),
            20,
        ))
}

fn in_ipv6_prefix(value: u128, network: u128, prefix_len: u32) -> bool {
    let mask = u128::MAX << (128 - prefix_len);
    value & mask == network & mask
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
            "0.1.2.3",           // this-network block
            "192.0.0.1",         // IETF protocol assignments
            "192.0.2.1",         // documentation
            "198.18.0.1",        // benchmarking
            "198.51.100.1",      // documentation
            "203.0.113.1",       // documentation
            "224.0.0.1",         // multicast
            "240.0.0.1",         // reserved
            "[64:ff9b::7f00:1]", // NAT64 transition prefix
            "[2001:db8::1]",     // documentation
            "[2002:7f00:1::]",   // 6to4 transition address
            "[3fff::1]",         // documentation
            "[ff02::1]",         // multicast
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
            "192.0.0.9",
            "192.0.0.10",
            "192.31.196.1",
            "192.52.193.1",
            "192.175.48.1",
            "[2606:4700:4700::1111]",
            "[2001:4860:4860::8888]",
            "[2001:1::1]",
            "[2001:1::2]",
            "[2001:1::3]",
            "[2001:3::1]",
            "[2001:4:112::1]",
            "[2001:20::1]",
            "[2001:30::1]",
            "api.openai.com",
        ] {
            assert_eq!(classify_host(h), HostClass::Public, "{h}");
            assert!(!classify_host(h).is_internal(), "{h}");
        }
    }

    #[test]
    fn special_purpose_literals_are_never_public() {
        for host in [
            "0.1.2.3",
            "10.0.0.1",
            "198.18.1.1",
            "224.0.0.1",
            "64:ff9b::7f00:1",
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(classify_host(host).is_internal(), "{host}");
        }
        for host in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert_eq!(classify_host(host), HostClass::Public, "{host}");
        }
    }

    #[test]
    fn generated_ipv4_special_purpose_ranges_are_never_public() {
        // Exercise interior points, not only the familiar first address from
        // each IANA special-purpose block. The deterministic generator keeps
        // the test cheap while covering host bits throughout large prefixes.
        let ranges = [
            (Ipv4Addr::new(0, 0, 0, 0), 8),
            (Ipv4Addr::new(10, 0, 0, 0), 8),
            (Ipv4Addr::new(100, 64, 0, 0), 10),
            (Ipv4Addr::new(127, 0, 0, 0), 8),
            (Ipv4Addr::new(169, 254, 0, 0), 16),
            (Ipv4Addr::new(172, 16, 0, 0), 12),
            (Ipv4Addr::new(192, 0, 0, 0), 24),
            (Ipv4Addr::new(192, 0, 2, 0), 24),
            (Ipv4Addr::new(192, 88, 99, 0), 24),
            (Ipv4Addr::new(192, 168, 0, 0), 16),
            (Ipv4Addr::new(198, 18, 0, 0), 15),
            (Ipv4Addr::new(198, 51, 100, 0), 24),
            (Ipv4Addr::new(203, 0, 113, 0), 24),
            (Ipv4Addr::new(224, 0, 0, 0), 4),
            (Ipv4Addr::new(240, 0, 0, 0), 4),
        ];
        let globally_reachable_exceptions = [
            u32::from(Ipv4Addr::new(192, 0, 0, 9)),
            u32::from(Ipv4Addr::new(192, 0, 0, 10)),
        ];

        let mut state = 0x9e37_79b9_u32;
        for (network, prefix_len) in ranges {
            let mask = u32::MAX << (32 - prefix_len);
            let network = u32::from(network) & mask;
            for _ in 0..2048 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let candidate = network | (state & !mask);
                if globally_reachable_exceptions.contains(&candidate) {
                    continue;
                }
                let host = Ipv4Addr::from(candidate).to_string();
                assert!(
                    classify_host(&host).is_internal(),
                    "special-purpose IPv4 escaped policy: {host}/{prefix_len}"
                );
            }
        }
    }

    #[test]
    fn generated_ipv6_special_purpose_ranges_are_never_public() {
        // These are the non-global or endpoint-transforming IPv6 allocations
        // relevant to outbound URL authorization. NAT64 is deliberately
        // denied even where the registry calls it globally reachable: the
        // embedded IPv4 endpoint can otherwise bypass the IPv4 policy.
        let ranges = [
            (Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
            (Ipv6Addr::new(0x0064, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
            (Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, 0, 0), 64),
            (Ipv6Addr::new(0x0100, 0, 0, 1, 0, 0, 0, 0), 64),
            (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32),
            (Ipv6Addr::new(0x2001, 2, 0, 0, 0, 0, 0, 0), 48),
            (Ipv6Addr::new(0x2001, 0x10, 0, 0, 0, 0, 0, 0), 28),
            (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
            (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
            (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
            (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16),
            (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
            (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
            (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
        ];

        let mut state = 0x6a09_e667_f3bc_c909_bb67_ae85_84ca_a73b_u128;
        for (network, prefix_len) in ranges {
            let mask = u128::MAX << (128 - prefix_len);
            let network = u128::from(network) & mask;
            for _ in 0..2048 {
                state = state
                    .wrapping_mul(0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645)
                    .wrapping_add(0x9e37_79b9_7f4a_7c15_6a09_e667_f3bc_c909);
                let host = Ipv6Addr::from(network | (state & !mask)).to_string();
                assert!(
                    classify_host(&host).is_internal(),
                    "special-purpose IPv6 escaped policy: {host}/{prefix_len}"
                );
            }
        }
    }
}
