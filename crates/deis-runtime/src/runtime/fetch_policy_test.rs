#[cfg(test)]
mod tests {
    use crate::runtime::fetch_policy::*;
    use std::net::IpAddr;

    fn policy() -> FetchPolicy {
        FetchPolicy::default()
    }

    /// A destination the policy allows, chosen so no test needs a network:
    /// the address is judged, not dialled.
    const PUBLIC: &str = "http://93.184.216.34/";

    async fn refusal(policy: &FetchPolicy, url: &str) -> Refusal {
        policy
            .check(url)
            .await
            .expect_err(&format!("{url} should have been refused"))
    }

    #[tokio::test]
    async fn a_public_address_is_allowed() {
        policy()
            .check(PUBLIC)
            .await
            .expect("a public address is the one thing an edge platform must allow");
    }

    #[tokio::test]
    async fn loopback_is_refused_by_literal_address() {
        // The whole point: deis-store listens here and trusts its tenant
        // header, so an app that could reach it could read any other app's
        // data.
        for url in [
            "http://127.0.0.1:8090/",
            "http://127.9.9.9/",
            "http://[::1]:8090/",
            // The same machine, spelled as IPv6. Without the mapped-address
            // unwrapping this one walks straight through.
            "http://[::ffff:127.0.0.1]:8090/",
        ] {
            match refusal(&policy(), url).await {
                // The mapped form is refused through its transition variant,
                // which still says which inner address and why.
                Refusal::BlockedAddress { kind, .. }
                | Refusal::BlockedTransitionAddress { kind, .. } => {
                    assert!(kind.contains("loopback"), "{url} was refused as {kind}");
                }
                other => panic!("{url} was refused as {other:?}, expected a loopback refusal"),
            }
        }
    }

    #[tokio::test]
    async fn the_private_ranges_are_refused() {
        for (url, expected) in [
            ("http://10.0.0.1/", "private"),
            ("http://172.16.4.4/", "private"),
            ("http://192.168.1.1:3000/", "private"),
            ("http://[fd00::1]/", "unique-local"),
            // Where a cloud instance answers with its own credentials.
            ("http://169.254.169.254/latest/meta-data/", "link-local"),
            ("http://[fe80::1]/", "link-local"),
            ("http://0.0.0.0:8090/", "unspecified"),
            ("http://100.64.0.1/", "carrier-grade NAT"),
            ("http://224.0.0.1/", "multicast"),
        ] {
            match refusal(&policy(), url).await {
                Refusal::BlockedAddress { kind, .. } => assert!(
                    kind.contains(expected),
                    "{url} was refused as {kind}, expected something about {expected}"
                ),
                other => panic!("{url} was refused as {other:?}, expected a blocked address"),
            }
        }
    }

    /// The transition forms. An IPv6 address that carries an IPv4 address
    /// inside it -- NAT64, 6to4 -- reaches the machine that inner address
    /// names, so it is judged as that address, and the refusal names both
    /// spellings.
    #[tokio::test]
    async fn the_transition_forms_are_judged_by_the_ipv4_they_carry() {
        for (url, embedded, expected) in [
            // NAT64, well-known prefix. `7f00:1` is 127.0.0.1: with a NAT64
            // gateway on the network this *is* a call to this node's own
            // loopback, which is exactly what the rule exists for.
            ("http://[64:ff9b::7f00:1]/", "127.0.0.1", "loopback"),
            ("http://[64:ff9b::a00:1]/", "10.0.0.1", "private"),
            // NAT64, local-use prefix (RFC 8215).
            ("http://[64:ff9b:1::7f00:1]/", "127.0.0.1", "loopback"),
            ("http://[64:ff9b:1::a00:1]/", "10.0.0.1", "private"),
            // 6to4: the inner address in bits 16-48.
            ("http://[2002:7f00:1::1]/", "127.0.0.1", "loopback"),
            ("http://[2002:a00:1::]/", "10.0.0.1", "private"),
            // The mapped form, judged the same way.
            ("http://[::ffff:10.0.0.1]/", "10.0.0.1", "private"),
        ] {
            match refusal(&policy(), url).await {
                Refusal::BlockedTransitionAddress {
                    embedded: found,
                    kind,
                    ..
                } => {
                    let expected_v4 = embedded.parse::<std::net::Ipv4Addr>().unwrap();
                    assert_eq!(found, expected_v4, "{url} carries {found}, not {embedded}");
                    assert!(
                        kind.contains(expected),
                        "{url} was refused as {kind}, expected something about {expected}"
                    );
                    // The refusal names the inner address, so whoever reads
                    // the message sees which machine was actually refused.
                    let text = refusal(&policy(), url).await.to_string();
                    let literal = url
                        .trim_start_matches("http://")
                        .trim_end_matches('/')
                        .trim_matches(['[', ']']);
                    assert!(
                        text.contains(embedded) && text.contains(literal),
                        "the refusal must name both the literal and the inner address: {text}"
                    );
                }
                other => panic!(
                    "{url} was refused as {other:?}, expected a transition-form refusal"
                ),
            }
        }
    }

    /// Teredo and the discard-only range carry no address that can be judged
    /// (Teredo's halves are obfuscated), so they are refused outright.
    #[tokio::test]
    async fn teredo_and_the_discard_only_range_are_refused_outright() {
        for (url, expected) in [
            ("http://[2001:0:5555:0:0:0:7f00:1]/", "Teredo"),
            ("http://[100::1]/", "discard-only"),
        ] {
            match refusal(&policy(), url).await {
                Refusal::BlockedAddress { kind, .. } => assert!(
                    kind.contains(expected),
                    "{url} was refused as {kind}, expected something about {expected}"
                ),
                other => panic!("{url} was refused as {other:?}, expected an outright refusal"),
            }
        }
    }

    /// DNS64/NAT64 is how an IPv6-only node reaches the public IPv4 internet,
    /// so a transition form carrying a public address is the ordinary case
    /// and must go through.
    #[tokio::test]
    async fn a_transition_form_carrying_a_public_ipv4_address_is_allowed() {
        policy()
            .check("http://[64:ff9b::808:808]/")
            .await
            .expect("8.8.8.8 through a NAT64 gateway is public");
        policy()
            .check("http://[2002:808:808::]/")
            .await
            .expect("8.8.8.8 through 6to4 is public");
    }

    /// `js/bootstrap.js` follows the redirect chain itself and checks every
    /// `Location:` with the same policy. The first hop here is a destination
    /// the operator opened; the second is a NAT64 spelling of loopback, which
    /// must be judged by the address it carries.
    #[tokio::test]
    async fn a_redirect_hop_into_a_transition_form_is_refused() {
        let policy =
            FetchPolicy::new(["93.184.216.34"], Vec::<String>::new()).expect("the entry parses");
        policy
            .check("http://93.184.216.34/")
            .await
            .expect("the operator opened the first hop");

        // The upstream answers 302 with
        // `Location: http://[64:ff9b::7f00:1]/`, and bootstrap.js hands that
        // URL back to the guard for the next hop.
        match refusal(&policy, "http://[64:ff9b::7f00:1]/").await {
            Refusal::BlockedTransitionAddress { embedded, kind, .. } => {
                assert_eq!(embedded, "127.0.0.1".parse::<std::net::Ipv4Addr>().unwrap());
                assert!(kind.contains("loopback"), "{kind}");
            }
            other => panic!(
                "the hop was refused as {other:?}, expected a transition-form refusal"
            ),
        }
    }

    /// The case a hostname-only policy gets wrong. `localhost` is a name, and
    /// it resolves to the loopback address; a policy that read the string
    /// would see a name it had no rule about and wave it through.
    #[tokio::test]
    async fn a_name_that_resolves_to_loopback_is_refused() {
        match refusal(&policy(), "http://localhost:8090/kv").await {
            Refusal::BlockedAddress { addr, kind } => {
                assert!(addr.is_loopback(), "localhost resolved to {addr}");
                assert!(kind.contains("loopback"));
            }
            other => panic!("localhost was refused as {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_platform_endpoint_is_refused_even_when_the_allowlist_covers_it() {
        // An operator who opens up their whole internal subnet, or who names
        // the store's host by hand, must still not get an app into deis-store.
        let policy = FetchPolicy::new(
            ["127.0.0.1", "store.internal"],
            ["http://127.0.0.1:8090", "http://store.internal:8090"],
        )
        .expect("the entries parse");

        assert_eq!(
            refusal(&policy, "http://127.0.0.1:8090/kv/get").await,
            Refusal::PlatformService
        );
        assert_eq!(
            refusal(&policy, "http://store.internal:8090/kv/get").await,
            Refusal::PlatformService
        );
    }

    /// The node's own address on a public interface (or the public address a
    /// cloud 1:1 NAT maps onto it) is a route to the data port just like
    /// loopback: the dispatcher trusts the `x-deis-app-id` header it is
    /// handed, so a worker that could call the port over that address could
    /// invoke another tenant's code on this node. The startup code enumerates
    /// the interface addresses and takes the operator-configured
    /// `node.advertised_addresses` on top; this holds the policy to its half
    /// of that promise -- a platform endpoint spelled as a non-loopback
    /// address is refused as such.
    #[tokio::test]
    async fn a_platform_endpoint_on_a_non_loopback_address_of_the_node_is_refused() {
        let policy = FetchPolicy::new(
            Vec::<String>::new(),
            ["http://203.0.113.7:8000", "http://[2001:db8::10]:8000"],
        )
        .expect("the entries parse");

        assert_eq!(
            refusal(&policy, "http://203.0.113.7:8000/").await,
            Refusal::PlatformService
        );
        assert_eq!(
            refusal(&policy, "http://[2001:db8::10]:8000/").await,
            Refusal::PlatformService
        );
        // ... while a neighbour at the next address is just a public host.
        assert!(matches!(
            refusal(&policy, "http://203.0.113.8:8000/").await,
            Refusal::BlockedAddress { .. }
        ));

        // An entry that names a port is port-exact, like every platform
        // entry: the generated ones carry the node's own http_port, and other
        // ports on that address are judged by the address ranges alone.
        assert!(matches!(
            refusal(&policy, "http://203.0.113.7:9000/").await,
            Refusal::BlockedAddress { .. }
        ));
    }

    #[tokio::test]
    async fn an_allowlisted_internal_service_is_reachable() {
        let policy =
            FetchPolicy::new(["10.4.0.7:8443"], Vec::<String>::new()).expect("the entry parses");

        policy
            .check("https://10.4.0.7:8443/orders")
            .await
            .expect("the operator opened this one up");

        // ... and only that one. The allowlist is not a door held open for
        // the rest of the range.
        assert!(matches!(
            refusal(&policy, "https://10.4.0.8:8443/orders").await,
            Refusal::BlockedAddress { .. }
        ));
        // ... nor for another port on the same host, because the entry named
        // a port.
        assert!(matches!(
            refusal(&policy, "https://10.4.0.7:9000/orders").await,
            Refusal::BlockedAddress { .. }
        ));
    }

    #[tokio::test]
    async fn an_allowlist_entry_without_a_port_covers_every_port() {
        let policy = FetchPolicy::new(["10.4.0.7"], Vec::<String>::new()).expect("it parses");
        policy.check("https://10.4.0.7:8443/").await.expect("8443");
        policy.check("http://10.4.0.7/").await.expect("80");
    }

    #[tokio::test]
    async fn only_http_schemes_reach_the_network() {
        // `file:` is the node's own disk. deno_fetch's default handler refuses
        // it too, but a refusal an application can read beats a bare network
        // error, and this does not depend on that handler staying the default.
        assert_eq!(
            refusal(&policy(), "file:///etc/passwd").await,
            Refusal::Scheme("file".to_string())
        );
        // A relative URL never became an absolute one, so there is nothing to
        // judge. Refusing is the safe reading: deno would otherwise resolve it
        // against the isolate's `file:///` location.
        assert!(matches!(
            refusal(&policy(), "/etc/passwd").await,
            Refusal::Unaddressable(_)
        ));
        // These two never leave the isolate, so a network policy has nothing
        // to say about them.
        policy()
            .check("data:text/plain,hello")
            .await
            .expect("data: URLs are not network destinations");
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_refused_rather_than_dialled() {
        match policy().check("http://deislet-no-such-host.invalid/").await {
            Err(Refusal::Unresolvable { host, .. }) => {
                assert_eq!(host, "deislet-no-such-host.invalid");
            }
            other => panic!("expected an unresolvable refusal, got {other:?}"),
        }
    }

    // --- the redirect backstop ---------------------------------------------

    #[test]
    fn the_deny_list_covers_the_ranges_deno_can_be_told_about() {
        let denied = FetchPolicy::default().denied_net_ranges("redirects.production");

        for expected in [
            "127.0.0.0/8",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "100.64.0.0/10",
            "0.0.0.0/8",
            "224.0.0.0/4",
            "240.0.0.0/4",
            // Single addresses, which is all deno's descriptor can express of
            // IPv6; see the module comment on redirects.
            "[::1]",
            "[::]",
        ] {
            assert!(
                denied.iter().any(|d| d == expected),
                "{expected} is missing from the deny list: {denied:?}"
            );
        }

        // Nothing unparseable reaches deno: an IPv6 subnet has no spelling
        // there, and a descriptor it refuses fails the isolate's build.
        assert!(
            !denied.iter().any(|d| d.contains("::/") || d.contains("]/")),
            "an IPv6 subnet got into the deny list, which deno cannot parse: {denied:?}"
        );
    }

    #[test]
    fn an_address_the_operator_opened_is_punched_out_and_the_rest_of_its_range_stays() {
        // deno's deny list beats its allow list with no way to spell the
        // exception, so keeping 127.0.0.0/8 whole would refuse the very
        // address the operator opened - and dropping the range would leave
        // every other loopback address unchecked at redirect time, which on a
        // node running deis-store on 127.0.0.1:8090 is the whole problem back
        // again.
        let policy = FetchPolicy::new(["127.0.0.1"], Vec::<String>::new()).unwrap();
        let denied = policy.denied_net_ranges("opened.production");

        let covers = |addr: &str| {
            let addr: IpAddr = addr.parse().unwrap();
            denied.iter().any(|entry| {
                let (net, prefix) = match entry.split_once('/') {
                    Some((net, prefix)) => (net, prefix.parse::<u8>().unwrap()),
                    None => (entry.as_str(), 32),
                };
                let Ok(net) = net.parse::<std::net::Ipv4Addr>() else {
                    return false;
                };
                let shift = 32 - u32::from(prefix);
                let mask = if shift == 32 { 0 } else { u32::MAX << shift };
                match addr {
                    IpAddr::V4(v4) => u32::from(net) & mask == u32::from(v4) & mask,
                    IpAddr::V6(_) => false,
                }
            })
        };

        assert!(!covers("127.0.0.1"), "the opened address is still denied");
        assert!(
            covers("127.0.0.2"),
            "the rest of loopback stopped being denied"
        );
        assert!(
            covers("127.1.2.3"),
            "the rest of loopback stopped being denied"
        );
        assert!(
            covers("10.0.0.1"),
            "an untouched range stopped being denied"
        );
    }

    #[test]
    fn the_platform_endpoints_survive_the_hole_an_allowlisted_address_punches() {
        // Measured against a real node, not imagined: with `[fetch] allow =
        // ["127.0.0.1:8095"]` -- the shape scripts/demo.sh ships -- 127.0.0.1
        // comes out of the deny list whole, because deno's deny list has no
        // per-port exception. Every other port on it was then a legal redirect
        // hop, and a `Location:` pointing at deis-control's REST API was
        // followed and answered. Naming the endpoints puts them back.
        let policy = FetchPolicy::new(
            ["127.0.0.1:8095"],
            [
                "http://127.0.0.1:8085",
                "http://127.0.0.1:8091",
                "http://[::1]:8092",
                "deis-store.internal:8090",
            ],
        )
        .unwrap();
        let denied = policy.denied_net_ranges("punched.production");

        for expected in [
            "127.0.0.1:8085",
            "127.0.0.1:8091",
            "[::1]:8092",
            "deis-store.internal:8090",
            // The same two sockets, spelled as IPv4-mapped IPv6. deno matches
            // the host as the URL writes it, and without these a redirect to
            // `http://[::ffff:127.0.0.1]:8085/` was granted.
            "[::ffff:127.0.0.1]:8085",
            "[::ffff:127.0.0.1]:8091",
        ] {
            assert!(
                denied.iter().any(|entry| entry == expected),
                "{expected} is not in the deny list, so a redirect could reach it: {denied:?}"
            );
        }

        // The address the operator opened is still open: this must not have
        // been fixed by refusing the destination they asked for.
        assert!(
            !denied.iter().any(|entry| entry == "127.0.0.1"
                || entry == "127.0.0.0/8"
                || entry == "127.0.0.1:8095"),
            "the allowlisted destination was denied after all: {denied:?}"
        );

        // Every entry has to be one deno accepts, by the same call the isolate
        // makes: a descriptor deno cannot parse fails the build and the
        // application never loads.
        let parser = std::sync::Arc::new(
            deno_runtime::deno_permissions::RuntimePermissionDescriptorParser::new(
                sys_traits::impls::RealSys,
            ),
        );
        deno_runtime::deno_permissions::Permissions::from_options(
            parser.as_ref(),
            &deno_runtime::deno_permissions::PermissionsOptions {
                allow_net: Some(Vec::new()),
                deny_net: Some(denied.clone()),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("deno rejected the deny list {denied:?}: {e}"));
    }

    #[test]
    fn the_two_spellings_of_the_blocked_set_agree() {
        // `blocked_reason` is what an application is told and why;
        // `blocked_ranges` is what deno enforces on a redirect hop nothing
        // else sees. Two tables that drifted apart would mean a destination
        // refused by one layer and reachable through the other.
        for net in crate::runtime::fetch_policy::blocked_range_networks_for_test() {
            assert!(
                crate::runtime::fetch_policy::blocked_reason_for_test(net).is_some(),
                "{net} is in the deny table but the classifier allows it"
            );
        }

        for public in [
            "93.184.216.34",
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700:4700::1111",
        ] {
            let addr: IpAddr = public.parse().unwrap();
            assert!(
                crate::runtime::fetch_policy::blocked_reason_for_test(addr).is_none(),
                "{public} is a public address and the classifier refuses it"
            );
            assert!(
                !crate::runtime::fetch_policy::any_blocked_range_covers_for_test(addr),
                "{public} is a public address and a deny range covers it"
            );
        }
    }

    // --- the resolver that backs the policy --------------------------------
    //
    // `FetchPolicy::check` runs before the request is handed to the HTTP
    // client, so it cannot be the last word on a *name*: the client resolves
    // the name again when it opens the socket, and the second answer can
    // differ from the first. `PolicyResolver` is what the client resolves
    // through, so these assert the half an application cannot get in front of.

    use deno_runtime::deno_fetch::dns::Resolve;
    use hyper_util::client::legacy::connect::dns::Name;

    fn name(host: &str) -> Name {
        host.parse().expect("a hostname")
    }

    #[tokio::test]
    async fn the_resolver_drops_addresses_the_policy_refuses() {
        // Everything `localhost` resolves to is loopback, so nothing survives
        // and the connection cannot be opened at all. This is what closes the
        // rebinding window: a name that passed `check` a moment ago and now
        // answers with 127.0.0.1 gets no address to connect to.
        let resolver = PolicyResolver::new(FetchPolicy::default(), "rebind.production");
        let error = resolver
            .resolve(name("localhost"))
            .await
            .expect_err("every address localhost has is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn the_resolver_keeps_what_the_operator_allowlisted() {
        // Otherwise an allowlist entry would pass the gate in front and then
        // fail at connect time with an unexplained network error.
        let policy = FetchPolicy::new(["localhost"], Vec::<String>::new()).unwrap();
        let resolved = PolicyResolver::new(policy, "allowed.production")
            .resolve(name("localhost"))
            .await
            .expect("the operator opened this one up");
        assert!(resolved.count() > 0);
    }

    #[tokio::test]
    async fn a_port_scoped_name_refuses_other_ports_before_the_portless_resolver() {
        // hyper gives the resolver only `localhost`, not the destination port,
        // so the port-aware gate must reject every other port before a request
        // can reach it. The resolver may then keep the private answer for the
        // one port that was explicitly opened.
        let policy = FetchPolicy::new(["localhost:8443"], Vec::<String>::new()).unwrap();
        assert!(matches!(
            policy.check("http://localhost:9000/").await,
            Err(Refusal::PortNotAllowed { port: 9000, .. })
        ));
        let resolved = PolicyResolver::new(policy, "port-scope.production")
            .resolve(name("localhost"))
            .await
            .expect("the connector may resolve the hostname after the port-aware gate passed");
        assert!(resolved.count() > 0);
    }

    #[test]
    fn a_port_aware_connector_preserves_a_hostname_exception_exactly() {
        let policy = FetchPolicy::new(["private.example:8443"], Vec::<String>::new()).unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();

        policy
            .check_resolved_address("private.example", 8443, loopback)
            .expect("the exact hostname and port were opened");
        assert!(matches!(
            policy.check_resolved_address("private.example", 9000, loopback),
            Err(Refusal::BlockedAddress { .. })
        ));
        assert!(matches!(
            FetchPolicy::default().check_resolved_address("public.example", 80, loopback),
            Err(Refusal::BlockedAddress { .. })
        ));
    }

    #[tokio::test]
    async fn the_resolver_refuses_a_platform_service_host_whatever_the_allowlist_says() {
        let policy = FetchPolicy::new(["localhost"], ["http://localhost:8090"]).unwrap();
        let error = PolicyResolver::new(policy, "store-reader.production")
            .resolve(name("localhost"))
            .await
            .expect_err("a platform endpoint is refused even to an allowlisted host");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn the_resolver_drops_a_platform_address_an_allowlisted_name_rebinds_to() {
        // The platform's endpoints are configured by address on a real node
        // (`127.0.0.1:<port>`), not by name. An operator may also allowlist a
        // hostname. If that name resolves to a public address when `check`
        // runs and to the loopback address a platform service listens on when
        // the socket is opened -- a DNS rebind -- the allowlist must not carry
        // it through: "no allowlist entry can reach them" holds for the
        // connect-time answer too. `localhost` stands in for the rebound name;
        // it resolves to the loopback address every platform endpoint shares -
        // and on a dual-stack host to `::1` beside it, which is the same
        // machine and must be dropped with it.
        let policy = FetchPolicy::new(["localhost"], ["127.0.0.1:8091"]).unwrap();
        let error = PolicyResolver::new(policy, "store-reader.production")
            .resolve(name("localhost"))
            .await
            .expect_err("a platform address is refused even to an allowlisted name");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_platform_endpoint_is_reached_by_every_spelling_of_its_machine() {
        // A dual-stack resolver offers `::1` beside `127.0.0.1` for the same
        // name, the IPv4-mapped form reaches the same socket as the IPv4
        // address, and a service bound on the unspecified address answers at
        // every loopback address. Each of those is the platform's own
        // endpoint by another name, and an allowlisted name that resolves to
        // one of them at connect time is a rebind.
        let bound_v4 = FetchPolicy::new(["example.test"], ["127.0.0.1:8091"]).unwrap();
        for spelling in ["::1", "::ffff:127.0.0.1", "127.0.0.2"] {
            let addr: IpAddr = spelling.parse().unwrap();
            assert!(
                bound_v4.is_a_platform_service_address(addr, Some(8091)),
                "{spelling} reaches a service bound at 127.0.0.1"
            );
        }
        assert!(
            !bound_v4.is_a_platform_service_address("10.0.0.1".parse().unwrap(), Some(8091)),
            "a private address is another machine, and the allowlist may open it"
        );
        assert!(
            !bound_v4.is_a_platform_service_address("::1".parse().unwrap(), Some(9000)),
            "the port still has to match"
        );

        let bound_v6 = FetchPolicy::new(["example.test"], ["[::1]:8091"]).unwrap();
        assert!(bound_v6.is_a_platform_service_address("127.0.0.1".parse().unwrap(), Some(8091)));

        let bound_everywhere = FetchPolicy::new(["example.test"], ["0.0.0.0:8085"]).unwrap();
        assert!(bound_everywhere.is_a_platform_service_address("::1".parse().unwrap(), Some(8085)));
        assert!(bound_everywhere
            .is_a_platform_service_address("127.0.0.1".parse().unwrap(), Some(8085)));
    }

    #[test]
    fn a_malformed_allowlist_entry_is_an_error_not_a_shrug() {
        // Silently dropping the entry would look exactly like one that worked
        // until the day someone tried to use it.
        assert!(FetchPolicy::new(["not a host at all"], Vec::<String>::new()).is_err());
        assert!(FetchPolicy::new(["example.com:not-a-port"], Vec::<String>::new()).is_err());
        assert!(FetchPolicy::new([""], Vec::<String>::new()).is_err());
    }

    #[test]
    fn the_entry_forms_an_operator_might_write_all_parse() {
        FetchPolicy::new(
            [
                "example.com",
                "example.com:8443",
                "10.0.0.1",
                "10.0.0.1:8443",
                "[fd00::1]",
                "[fd00::1]:8443",
                "fd00::1",
                "http://example.com:8080",
            ],
            Vec::<String>::new(),
        )
        .expect("every one of these is a shape an operator writes");
    }

    #[test]
    fn a_credential_in_the_url_does_not_reach_the_log_line() {
        // A refusal is logged on the node and shipped to deis-telemetry.
        // `https://user:token@host/` is a shape applications use, and logging
        // it as given would put that token somewhere the application never
        // agreed to put it.
        let cleaned = loggable("https://someone:s3cret@api.example.com/orders?id=7");
        assert!(!cleaned.contains("s3cret"), "{cleaned}");
        assert!(!cleaned.contains("someone"), "{cleaned}");
        // What is left is still enough to work out which call was refused.
        assert!(cleaned.contains("api.example.com"), "{cleaned}");
        assert!(cleaned.contains("/orders"), "{cleaned}");

        // A URL with no credentials is passed through untouched, and so is
        // something that is not a URL at all - a refusal has to be able to
        // name what it refused.
        assert_eq!(
            loggable("https://api.example.com/orders"),
            "https://api.example.com/orders"
        );
        assert_eq!(loggable("not a url"), "not a url");
    }

    #[test]
    fn the_refusal_message_tells_the_application_what_to_do() {
        let refusal = Refusal::BlockedAddress {
            addr: "127.0.0.1".parse::<IpAddr>().unwrap(),
            kind: "a loopback address, which is this node itself",
        };
        let text = refusal.to_string();
        assert!(text.contains("127.0.0.1"), "{text}");
        assert!(text.contains("[fetch] allow"), "{text}");
    }
}
