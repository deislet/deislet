#[cfg(test)]
mod tests {
    use super::super::*;
    use sha2::Digest;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// `AppServices` holds gRPC clients and cannot derive `Debug`, so the
    /// `expect` family is unavailable; this keeps the failure message useful.
    fn connect_or_panic(tenant: &str, wiring: &ServiceWiring) -> AppServices {
        match AppServices::connect(&crate::runtime::tenant::test_tenant(tenant), wiring) {
            Ok(services) => services,
            Err(error) => panic!("connecting services for {tenant} failed: {error}"),
        }
    }

    fn manifest(json: &str) -> deis_bundle::Manifest {
        deis_bundle::Manifest::parse(&deis_bundle::complete_manifest_json(json))
            .expect("test manifest parses")
    }

    #[test]
    fn reserved_keys_are_taken_out_of_the_app_env() {
        // The app must not see platform wiring among its own variables: it is
        // not the app's configuration, and an app variable spelled like one
        // of these must not be able to point a client somewhere else.
        let mut map = env(&[
            ("DEIS_SERVICES", "kv,queue"),
            ("DEIS_STORE_ENDPOINT", "http://127.0.0.1:8090"),
            ("DEIS_QUEUE_ENDPOINT", "http://127.0.0.1:8091"),
            ("DEIS_QUEUE_CONSUMERS", "jobs:5"),
            ("LOG_LEVEL", "info"),
        ]);

        let wiring = ServiceWiring::take_from_env(&mut map);

        assert_eq!(map, env(&[("LOG_LEVEL", "info")]));
        assert!(wiring.declaration.declares(ServiceKind::Kv));
        assert!(wiring.declaration.declares(ServiceKind::Queue));
        assert!(!wiring.declaration.declares(ServiceKind::Blob));
        assert_eq!(
            wiring.endpoints.store.as_deref(),
            Some("http://127.0.0.1:8090")
        );
        assert_eq!(
            wiring.consumers,
            vec![QueueConsumerSpec {
                name: "jobs".to_string(),
                batch_size: Some(5),
                max_concurrency: None,
            }]
        );
    }

    /// The consumer entries are positional, so a queue that named only its
    /// concurrency still has to occupy the batch-size slot.
    ///
    /// A round trip rather than a string comparison, because what matters is
    /// that the writer and the reader agree: the node writes this map out of
    /// the version's manifest and reads it back to start its pollers, and a
    /// disagreement would show up as a consumer quietly running with the
    /// wrong numbers rather than as a failure.
    #[test]
    fn a_consumer_carries_its_concurrency_through_the_env_encoding() {
        let original = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["queue"]),
            endpoints: ServiceEndpoints {
                store: None,
                queue: Some("http://queue:8091".to_string()),
            },
            storage_token: None,
            consumers: vec![
                QueueConsumerSpec {
                    name: "jobs".to_string(),
                    batch_size: Some(10),
                    max_concurrency: Some(3),
                },
                // The awkward one: no batch size, but a concurrency after it.
                QueueConsumerSpec {
                    name: "emails".to_string(),
                    batch_size: None,
                    max_concurrency: Some(2),
                },
            ],
            ..ServiceWiring::default()
        };

        let mut map = HashMap::new();
        original.write_to_env(&mut map);
        assert_eq!(
            map.get(QUEUE_CONSUMERS_KEY).map(String::as_str),
            Some("jobs:10:3,emails:0:2"),
            "0 is the spelling for a batch size nobody declared"
        );

        let read_back = ServiceWiring::take_from_env(&mut map);
        assert_eq!(read_back.consumers, original.consumers);
    }

    #[test]
    fn an_env_without_reserved_keys_declares_nothing() {
        let mut map = env(&[("LOG_LEVEL", "info")]);
        let wiring = ServiceWiring::take_from_env(&mut map);
        assert!(wiring.is_empty());
        assert_eq!(wiring.endpoints, ServiceEndpoints::default());
        assert_eq!(map.len(), 1, "the app's own variables are untouched");
    }

    #[test]
    fn the_env_encoding_round_trips() {
        // The control plane writes the map and the node reads it; if the two
        // halves disagree the symptom is a service that is silently absent,
        // so they are the same code and this test holds them together.
        let original = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv", "database", "blob", "queue"]),
            endpoints: ServiceEndpoints {
                store: Some("http://store:8090".to_string()),
                queue: Some("http://queue:8091".to_string()),
            },
            consumers: vec![
                QueueConsumerSpec {
                    name: "jobs".to_string(),
                    batch_size: Some(10),
                    max_concurrency: None,
                },
                QueueConsumerSpec {
                    name: "emails".to_string(),
                    batch_size: None,
                    max_concurrency: None,
                },
            ],
            ..ServiceWiring::default()
        };

        let mut map = HashMap::new();
        original.write_to_env(&mut map);
        let decoded = ServiceWiring::take_from_env(&mut map);

        assert_eq!(decoded, original);
        assert!(map.is_empty(), "every key written is a key read back");
    }

    #[test]
    fn an_empty_wiring_writes_no_keys() {
        let mut map = HashMap::new();
        ServiceWiring::default().write_to_env(&mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn a_blank_endpoint_counts_as_absent() {
        // An unset variable often arrives as an empty string; treating that
        // as an address would produce a client that can never connect.
        let mut map = env(&[("DEIS_STORE_ENDPOINT", "   ")]);
        let wiring = ServiceWiring::take_from_env(&mut map);
        assert_eq!(wiring.endpoints.store, None);
    }

    #[test]
    fn an_unknown_service_name_is_skipped() {
        let declaration = ServiceDeclaration::from_names(["kv", "telepathy"]);
        assert!(declaration.declares(ServiceKind::Kv));
        assert_eq!(declaration.to_csv(), "kv");
    }

    #[test]
    fn a_consumer_with_an_unreadable_batch_size_keeps_the_queue() {
        // Losing the size means polling with the default; losing the queue
        // would mean never consuming it at all.
        let mut map = env(&[("DEIS_QUEUE_CONSUMERS", "jobs:many,emails:2")]);
        let wiring = ServiceWiring::take_from_env(&mut map);
        assert_eq!(
            wiring.consumers,
            vec![
                QueueConsumerSpec {
                    name: "jobs".to_string(),
                    batch_size: None,
                    max_concurrency: None,
                },
                QueueConsumerSpec {
                    name: "emails".to_string(),
                    batch_size: Some(2),
                    max_concurrency: None,
                },
            ]
        );
    }

    #[test]
    fn a_manifest_declares_only_the_services_set_to_true() {
        let wiring = ServiceWiring::from_manifest(
            &manifest(
                r#"{
                    "schema": 1,
                    "app": "demo",
                    "entry": "worker/index.js",
                    "services": { "kv": true, "database": false, "blob": false, "queue": true },
                    "queues": [{ "name": "jobs", "batch_size": 4 }]
                }"#,
            ),
            ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
        );

        assert_eq!(wiring.declaration.to_csv(), "kv,queue");
        assert_eq!(wiring.consumers.len(), 1);
        assert_eq!(wiring.consumers[0].batch_size, Some(4));
    }

    #[tokio::test]
    async fn only_declared_services_get_a_client() {
        // An app that never asked for a database must not hold a connection
        // to one.
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
        };

        let services = connect_or_panic("demo.production", &wiring);
        assert!(services.kv.get("").is_some());
        assert!(services.database.get("").is_none());
        assert!(services.blob.get("").is_none());
        assert!(services.queue.is_none(), "queue was not declared");
    }

    #[tokio::test]
    async fn every_declared_service_gets_a_client_stamped_with_the_tenant() {
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv", "database", "blob", "queue"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
        };

        // Every client is charged to the pair, not to the application: a
        // preview that stamped only "demo-app" would read and overwrite what
        // production stored.
        let services = connect_or_panic("demo-app.preview-7", &wiring);
        let tenant = crate::runtime::tenant::test_tenant("demo-app.preview-7");
        assert_eq!(
            services.kv.get("").map(|c| c.tenant()),
            Some(tenant.as_str())
        );
        assert_eq!(
            services.database.get("").map(|c| c.tenant()),
            Some(tenant.as_str())
        );
        assert_eq!(
            services.blob.get("").map(|c| c.tenant()),
            Some(tenant.as_str())
        );
        assert_eq!(
            services.queue.as_ref().map(|c| c.tenant()),
            Some(tenant.as_str())
        );
    }

    #[test]
    fn an_identity_that_is_not_a_tenant_is_refused_rather_than_sent_to_production() {
        // The pair is the identity of a running application. A bare app id
        // would have to be given some environment, and the only one available
        // to guess is production -- which is exactly the namespace a preview
        // must never touch.
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
        };

        let error = match AppServices::connect("demo", &wiring) {
            Ok(_) => panic!("a bare application id is not a canonical tenant and must be refused"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("not a canonical storage tenant"),
            "{error}"
        );
    }

    #[test]
    fn declaring_a_service_without_an_address_yields_no_client() {
        // The app keeps running; every call answers "service not available"
        // rather than the node refusing to load the app.
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints::default(),
            consumers: Vec::new(),
            ..ServiceWiring::default()
        };
        let services = connect_or_panic("demo.production", &wiring);
        assert!(services.is_empty());
    }

    #[test]
    fn a_malformed_address_is_refused_when_the_clients_are_built() {
        // Unlike a store that is merely down, this can never start working,
        // so it is reported at load time instead of on every call.
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("not a url".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
        };
        let error = match AppServices::connect(
            &crate::runtime::tenant::test_tenant("demo.production"),
            &wiring,
        ) {
            Ok(_) => panic!("an address that is not a URL must be refused"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("not a url"), "{error}");
    }

    #[tokio::test]
    async fn an_unreachable_store_still_produces_a_client() {
        // Nothing listens on port 1. Building the client must still succeed:
        // a storage outage may not stop an app from loading, and the binding
        // has to recover by itself once the store is back.
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:1".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
        };
        let services = match AppServices::connect(
            &crate::runtime::tenant::test_tenant("demo.production"),
            &wiring,
        ) {
            Ok(services) => services,
            Err(error) => panic!("a lazy client is built even when nothing answers: {error}"),
        };
        assert!(services.kv.get("").is_some());
    }

    #[test]
    fn a_tenant_that_cannot_be_a_header_is_refused() {
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
        };
        assert!(AppServices::connect("bad\nid.production", &wiring).is_err());
    }

    #[test]
    fn consumers_are_not_started_without_a_queue_address() {
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["queue"]),
            endpoints: ServiceEndpoints::default(),
            consumers: vec![QueueConsumerSpec {
                name: "jobs".to_string(),
                batch_size: None,
                max_concurrency: None,
            }],
            ..ServiceWiring::default()
        };
        assert!(wiring.active_consumers("demo.production").is_empty());
    }

    #[test]
    fn consumers_are_not_started_without_the_queue_service_declared() {
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::new(),
            endpoints: ServiceEndpoints {
                store: None,
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
            consumers: vec![QueueConsumerSpec {
                name: "jobs".to_string(),
                batch_size: None,
                max_concurrency: None,
            }],
            ..ServiceWiring::default()
        };
        assert!(wiring.active_consumers("demo.production").is_empty());
    }

    #[test]
    fn consumers_are_started_when_both_halves_are_present() {
        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["queue"]),
            endpoints: ServiceEndpoints {
                store: None,
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
            consumers: vec![QueueConsumerSpec {
                name: "jobs".to_string(),
                batch_size: None,
                max_concurrency: None,
            }],
            ..ServiceWiring::default()
        };
        assert_eq!(wiring.active_consumers("demo.production").len(), 1);
    }

    #[test]
    fn a_deployment_env_keeps_the_control_plane_addresses_and_gains_the_manifest_half() {
        // The addresses come from the control plane and the declaration comes
        // from the bundle; this is the one place the two meet, so a mistake
        // here is a service that is silently never bound.
        let mut map = env(&[
            ("DEIS_STORE_ENDPOINT", "http://store:8090"),
            ("DEIS_QUEUE_ENDPOINT", "http://queue:8091"),
            ("LOG_LEVEL", "info"),
        ]);

        wire_deployment_env(
            &manifest(
                r#"{
                    "schema": 1,
                    "app": "demo",
                    "entry": "worker/index.js",
                    "services": { "kv": true, "queue": true },
                    "queues": [{ "name": "jobs", "batch_size": 3 }]
                }"#,
            ),
            &mut map,
        );

        assert_eq!(
            map.get("DEIS_SERVICES").map(String::as_str),
            Some("kv,queue")
        );
        assert_eq!(
            map.get("DEIS_QUEUE_CONSUMERS").map(String::as_str),
            Some("jobs:3")
        );

        let wiring = ServiceWiring::take_from_env(&mut map);
        assert_eq!(wiring.endpoints.store.as_deref(), Some("http://store:8090"));
        assert_eq!(wiring.active_consumers("demo.production").len(), 1);
        assert_eq!(map, env(&[("LOG_LEVEL", "info")]));
    }

    #[test]
    fn a_deployment_env_is_never_a_source_of_capabilities() {
        // A node asks the control plane for its own capabilities. A value in
        // the env map spelled like the old token keys is an ordinary variable
        // of the application, and never the proof this node presents.
        let mut map = env(&[
            ("DEIS_STORE_ENDPOINT", "http://store:8090"),
            ("DEIS_QUEUE_ENDPOINT", "http://queue:8091"),
            ("DEIS_STORAGE_TOKEN", "forged"),
            ("DEIS_QUEUE_TOKEN", "forged"),
        ]);
        let dropped = strip_reserved_env(&mut map);
        assert_eq!(dropped, vec!["DEIS_STORE_ENDPOINT", "DEIS_QUEUE_ENDPOINT"]);

        map.insert(
            "DEIS_STORE_ENDPOINT".to_string(),
            "http://store:8090".to_string(),
        );
        map.insert(
            "DEIS_QUEUE_ENDPOINT".to_string(),
            "http://queue:8091".to_string(),
        );
        wire_deployment_env(
            &manifest(
                r#"{
                    "schema": 1,
                    "app": "demo",
                    "entry": "worker/index.js",
                    "services": { "kv": true, "queue": true }
                }"#,
            ),
            &mut map,
        );

        let wiring = ServiceWiring::take_from_env(&mut map);
        assert_eq!(wiring.storage_token, None);
        assert_eq!(wiring.queue_token, None);
        assert_eq!(
            map,
            env(&[
                ("DEIS_STORAGE_TOKEN", "forged"),
                ("DEIS_QUEUE_TOKEN", "forged")
            ])
        );
    }

    #[test]
    fn a_deployment_without_addresses_declares_services_but_binds_none() {
        let mut map = env(&[("LOG_LEVEL", "info")]);
        wire_deployment_env(
            &manifest(
                r#"{
                    "schema": 1,
                    "app": "demo",
                    "entry": "worker/index.js",
                    "services": { "kv": true }
                }"#,
            ),
            &mut map,
        );

        let wiring = ServiceWiring::take_from_env(&mut map);
        assert!(wiring.declaration.declares(ServiceKind::Kv));
        assert_eq!(wiring.endpoints.store, None);
    }

    #[test]
    fn a_manifest_cannot_leave_a_stale_service_list_behind() {
        // The previous version of this app declared kv and consumed a queue.
        // The new one declares neither. Writing without clearing first would
        // leave both keys standing: the isolate would be handed a KV client
        // for a service this bundle never asked for, and a poller would keep
        // leasing messages that nothing in the new code handles.
        let mut map = env(&[
            ("DEIS_SERVICES", "kv,queue"),
            ("DEIS_QUEUE_CONSUMERS", "jobs:3"),
            ("DEIS_STORE_ENDPOINT", "http://store:8090"),
            ("LOG_LEVEL", "info"),
        ]);

        wire_deployment_env(
            &manifest(
                r#"{
                    "schema": 1,
                    "app": "demo",
                    "entry": "worker/index.js"
                }"#,
            ),
            &mut map,
        );

        assert_eq!(map.get("DEIS_SERVICES"), None);
        assert_eq!(map.get("DEIS_QUEUE_CONSUMERS"), None);

        let wiring = ServiceWiring::take_from_env(&mut map);
        assert!(wiring.declaration.is_empty());
        assert!(wiring.consumers.is_empty());
        // The address survives: it is the control plane's half of the
        // wiring, and clearing it would unbind an app that still declares
        // storage in a later deployment.
        assert_eq!(wiring.endpoints.store.as_deref(), Some("http://store:8090"));
        assert_eq!(map, env(&[("LOG_LEVEL", "info")]));
    }

    #[test]
    fn a_bundle_cannot_choose_where_its_storage_client_connects() {
        // The failure this guards: a manifest that declares
        // DEIS_STORE_ENDPOINT itself, on a node whose delivery carried no
        // address to overwrite it with. Nothing else in the chain would
        // notice - the app loads, its calls succeed - and every read and
        // write goes to a host the bundle picked.
        let mut map = env(&[
            ("DEIS_STORE_ENDPOINT", "http://attacker.example:8090"),
            ("DEIS_QUEUE_ENDPOINT", "http://attacker.example:8091"),
            ("LOG_LEVEL", "info"),
        ]);
        let dropped = strip_reserved_env(&mut map);

        assert!(dropped.contains(&"DEIS_STORE_ENDPOINT"));
        assert!(dropped.contains(&"DEIS_QUEUE_ENDPOINT"));

        wire_deployment_env(
            &manifest(
                r#"{
                    "schema": 1,
                    "app": "demo",
                    "entry": "worker/index.js",
                    "services": { "kv": true }
                }"#,
            ),
            &mut map,
        );

        let wiring = ServiceWiring::take_from_env(&mut map);
        assert!(wiring.declaration.declares(ServiceKind::Kv));
        assert_eq!(
            wiring.endpoints.store, None,
            "the bundle's own address must not become the store this app talks to"
        );
        assert_eq!(map, env(&[("LOG_LEVEL", "info")]));
    }

    #[test]
    fn stripping_reserved_keys_leaves_the_applications_own_variables() {
        let mut map = env(&[("DEIS_SERVICES", "kv"), ("DEIS_LOG_LEVEL", "debug")]);
        let dropped = strip_reserved_env(&mut map);
        assert_eq!(dropped, vec!["DEIS_SERVICES"]);
        // A variable that merely starts with DEIS_ is the application's own.
        assert_eq!(map, env(&[("DEIS_LOG_LEVEL", "debug")]));
    }

    #[test]
    fn a_blank_token_counts_as_absent() {
        // An empty header would be presented as a token and refused, which
        // reads as a bad credential rather than as none yet.
        let identity = StorageIdentity::new(
            crate::runtime::tenant::test_tenant("demo.production"),
            Some("   "),
        )
        .expect("a tenant");
        assert!(!identity.is_proven());
    }

    #[test]
    fn a_storage_call_carries_the_proof_and_no_tenant_header() {
        // The data services take the tenant from the capability, so the
        // capability is the whole of what a call has to carry.
        let identity = StorageIdentity::new(
            crate::runtime::tenant::test_tenant("demo.production"),
            Some("a-minted-token"),
        )
        .expect("a legal tenant and a sendable token");
        let request = identity.stamp(());

        let header = |name: &str| {
            request
                .metadata()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        assert_eq!(header("x-deis-tenant"), None);
        assert_eq!(
            header("authorization").as_deref(),
            Some("Bearer a-minted-token")
        );
        assert!(identity.is_proven());
    }

    #[test]
    fn a_node_with_no_token_yet_sends_no_authorization() {
        let identity =
            StorageIdentity::new(crate::runtime::tenant::test_tenant("demo.production"), None)
                .expect("a legal tenant");
        let request = identity.stamp(());
        assert!(request.metadata().get("authorization").is_none());
        assert!(!identity.is_proven());
    }

    #[test]
    fn a_token_that_could_not_be_sent_is_refused_when_the_client_is_built() {
        // A value that is not sendable metadata can only be a control plane
        // that handed this node something that was never a token, and every
        // storage call would fail with a message about the tenant instead.
        let error = StorageIdentity::new(
            crate::runtime::tenant::test_tenant("demo.production"),
            Some("line\nbreak"),
        )
        .expect_err("a token with a control character cannot be sent");
        assert!(
            error
                .to_string()
                .contains("cannot be sent as gRPC metadata"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_delivered_token_reaches_the_store_through_the_clients_this_module_builds() {
        // The node's half of the chain, end to end and over a real socket:
        // the token the node holds for its tenant comes back out of a client
        // this module built as the proof
        // deis-store demands. Asserting the field alone would not catch a
        // client that holds the token and never sends it.
        const SECRET: &str = "the-secret-both-halves-hold";
        let tenant = "demo.production";
        let store = crate::runtime::test_store::TestStore::start_with_secret(SECRET).await;

        let mut map = env(&[("DEIS_SERVICES", "kv")]);
        map.insert("DEIS_STORE_ENDPOINT".to_string(), store.endpoint.clone());
        let mut hasher = sha2::Sha256::new();
        sha2::Digest::update(&mut hasher, SECRET.as_bytes());
        let hash = sha2::Digest::finalize(hasher);
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&hash);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
        let claims = deis_capability::CapabilityClaims::new(
            "store",
            "spiffe://deislet/control/main",
            "spiffe://deislet/node/test",
            1,
            std::time::Duration::from_secs(900),
            vec![deis_capability::CapabilityResource {
                tenant: crate::runtime::tenant::test_tenant(tenant),
                binding: "kv".to_string(),
                generation: None,
            }],
        );
        let mut wiring = ServiceWiring::take_from_env(&mut map);
        wiring.storage_token = Some(deis_capability::encode(&claims, &signing_key));
        let services = connect_or_panic(tenant, &wiring);
        let kv = services
            .kv
            .get("")
            .cloned()
            .expect("kv was declared and an address was sent");

        kv.set("k".to_string(), b"v".to_vec())
            .await
            .expect("a client built from a delivery that carried a token is authorised");
        assert_eq!(
            kv.get("k".to_string()).await.expect("read back"),
            Some(b"v".to_vec())
        );
    }

    #[tokio::test]
    async fn a_delivery_without_a_token_is_refused_by_a_store_that_requires_one() {
        // The same wiring minus the token. The client is still built - an app
        // must load whatever the storage services think of it - and the
        // refusal arrives on the call, naming the credential rather than the
        // tenant.
        const SECRET: &str = "the-secret-both-halves-hold";
        let store = crate::runtime::test_store::TestStore::start_with_secret(SECRET).await;

        let mut map = env(&[("DEIS_SERVICES", "kv")]);
        map.insert("DEIS_STORE_ENDPOINT".to_string(), store.endpoint.clone());

        let wiring = ServiceWiring::take_from_env(&mut map);
        assert_eq!(wiring.storage_token, None);
        let services = connect_or_panic("demo.production", &wiring);
        let kv = services
            .kv
            .get("")
            .cloned()
            .expect("kv was declared and an address was sent");

        let error = kv
            .set("k".to_string(), b"v".to_vec())
            .await
            .expect_err("a store must not serve an unproven call");
        assert_eq!(
            error
                .downcast_ref::<tonic::Status>()
                .map(tonic::Status::code),
            Some(tonic::Code::Unauthenticated),
            "{error:#}"
        );
    }

    #[test]
    fn the_capability_token_never_reaches_a_log_line() {
        let wiring = ServiceWiring {
            storage_token: Some("a-minted-token".to_string()),
            ..ServiceWiring::default()
        };
        let rendered = format!("{wiring:?}");
        assert!(
            !rendered.contains("a-minted-token"),
            "the token is printable: {rendered}"
        );
        assert!(rendered.contains("<present>"), "{rendered}");

        let identity = StorageIdentity::new(
            crate::runtime::tenant::test_tenant("demo.production"),
            Some("a-minted-token"),
        )
        .expect("a legal tenant");
        let rendered = format!("{identity:?}");
        assert!(
            !rendered.contains("a-minted-token"),
            "the token is printable: {rendered}"
        );
    }

    #[test]
    fn every_reserved_key_is_one_the_reader_actually_removes() {
        // A key listed as reserved but not removed would reach the app as a
        // variable, which is exactly what the list exists to prevent.
        let mut map: HashMap<String, String> = RESERVED_ENV_KEYS
            .iter()
            .map(|key| (key.to_string(), "x".to_string()))
            .collect();
        ServiceWiring::take_from_env(&mut map);
        assert!(map.is_empty(), "left over: {map:?}");
    }
}
