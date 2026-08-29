#[cfg(test)]
mod tests {
    use super::super::*;
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
        match AppServices::connect(tenant, wiring) {
            Ok(services) => services,
            Err(error) => panic!("connecting services for {tenant} failed: {error}"),
        }
    }

    fn manifest(json: &str) -> deis_bundle::Manifest {
        deis_bundle::Manifest::parse(json).expect("test manifest parses")
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
                batch_size: Some(5)
            }]
        );
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
                },
                QueueConsumerSpec {
                    name: "emails".to_string(),
                    batch_size: None,
                },
            ],
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
                    batch_size: None
                },
                QueueConsumerSpec {
                    name: "emails".to_string(),
                    batch_size: Some(2)
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
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
            consumers: Vec::new(),
        };

        let services = connect_or_panic("demo.production", &wiring);
        assert!(services.kv.is_some());
        assert!(services.database.is_none());
        assert!(services.blob.is_none());
        assert!(services.queue.is_none(), "queue was not declared");
    }

    #[tokio::test]
    async fn every_declared_service_gets_a_client_stamped_with_the_tenant() {
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv", "database", "blob", "queue"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
            consumers: Vec::new(),
        };

        // Every client is charged to the pair, not to the application: a
        // preview that stamped only "demo-app" would read and overwrite what
        // production stored.
        let services = connect_or_panic("demo-app.preview-7", &wiring);
        assert_eq!(
            services.kv.as_ref().map(|c| c.tenant()),
            Some("demo-app.preview-7")
        );
        assert_eq!(
            services.database.as_ref().map(|c| c.tenant()),
            Some("demo-app.preview-7")
        );
        assert_eq!(
            services.blob.as_ref().map(|c| c.tenant()),
            Some("demo-app.preview-7")
        );
        assert_eq!(
            services.queue.as_ref().map(|c| c.tenant()),
            Some("demo-app.preview-7")
        );
    }

    #[test]
    fn an_identity_that_is_not_a_tenant_is_refused_rather_than_sent_to_production() {
        // The pair is the identity of a running application. A bare app id
        // would have to be given some environment, and the only one available
        // to guess is production -- which is exactly the namespace a preview
        // must never touch.
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
        };

        let error = match AppServices::connect("demo", &wiring) {
            Ok(_) => panic!("a bare application id names no environment and must be refused"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("not a storage tenant"),
            "{error}"
        );
    }

    #[test]
    fn declaring_a_service_without_an_address_yields_no_client() {
        // The app keeps running; every call answers "service not available"
        // rather than the node refusing to load the app.
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints::default(),
            consumers: Vec::new(),
        };
        let services = connect_or_panic("demo.production", &wiring);
        assert!(services.is_empty());
    }

    #[test]
    fn a_malformed_address_is_refused_when_the_clients_are_built() {
        // Unlike a store that is merely down, this can never start working,
        // so it is reported at load time instead of on every call.
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("not a url".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
        };
        let error = match AppServices::connect("demo.production", &wiring) {
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
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:1".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
        };
        let services = match AppServices::connect("demo.production", &wiring) {
            Ok(services) => services,
            Err(error) => panic!("a lazy client is built even when nothing answers: {error}"),
        };
        assert!(services.kv.is_some());
    }

    #[test]
    fn a_tenant_that_cannot_be_a_header_is_refused() {
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
        };
        assert!(AppServices::connect("bad\nid.production", &wiring).is_err());
    }

    #[test]
    fn consumers_are_not_started_without_a_queue_address() {
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["queue"]),
            endpoints: ServiceEndpoints::default(),
            consumers: vec![QueueConsumerSpec {
                name: "jobs".to_string(),
                batch_size: None,
            }],
        };
        assert!(wiring.active_consumers("demo.production").is_empty());
    }

    #[test]
    fn consumers_are_not_started_without_the_queue_service_declared() {
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::new(),
            endpoints: ServiceEndpoints {
                store: None,
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
            consumers: vec![QueueConsumerSpec {
                name: "jobs".to_string(),
                batch_size: None,
            }],
        };
        assert!(wiring.active_consumers("demo.production").is_empty());
    }

    #[test]
    fn consumers_are_started_when_both_halves_are_present() {
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["queue"]),
            endpoints: ServiceEndpoints {
                store: None,
                queue: Some("http://127.0.0.1:8091".to_string()),
            },
            consumers: vec![QueueConsumerSpec {
                name: "jobs".to_string(),
                batch_size: None,
            }],
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
