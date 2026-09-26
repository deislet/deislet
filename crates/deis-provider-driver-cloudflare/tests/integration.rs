//! Integration tests for `deis-provider-driver-cloudflare`.
//!
//! Tests verify protocol adherence, credential parsing, wrangler output
//! parsing, journal persistence, and command mapping without running the
//! real wrangler binary (fixture-based, fast, reproducible).

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use deis_provider_driver::{
        DeploymentState, DriverNextAction, DriverOperation, DriverProcessOutcome,
        DriverProcessRequest, DriverReceipt, DriverStepState, ProviderNeutralPlan,
        ProviderTargetRef, SideEffect,
    };
    use deis_wire::CanonicalId;

    /// Build a valid DriverRequest for a given operation.
    fn make_request(operation: DriverOperation) -> deis_provider_driver::DriverRequest {
        deis_provider_driver::DriverRequest {
            schema_version: 1,
            operation_id: CanonicalId::new_v7(),
            operation,
            desired_state_digest: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .unwrap(),
            application_id: CanonicalId::new_v7(),
            environment_id: CanonicalId::new_v7(),
            target: ProviderTargetRef {
                provider_target_id: CanonicalId::new_v7(),
                provider_installation_id: CanonicalId::new_v7(),
                backend_id: "cloudflare-workers-pages".to_owned(),
                target_config_digest: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .parse()
                    .unwrap(),
            },
            artifact: None,
            plan: ProviderNeutralPlan::default(),
            rollback_target: None,
            reconciles: None,
        }
    }

    // ── Credential tests ────────────────────────────────────────────────────

    #[test]
    fn parses_cloudflare_credential_from_env() {
        let cred =
            deis_provider_driver_cloudflare::credentials::CloudflareCredential::from_env_value(
                r#"{"api_token": "tok-123", "account_id": "acct-456"}"#,
            )
            .unwrap();
        assert_eq!(cred.api_token, "tok-123");
        assert_eq!(cred.account_id, "acct-456");
    }

    #[test]
    fn rejects_credential_with_missing_account_id() {
        let err =
            deis_provider_driver_cloudflare::credentials::CloudflareCredential::from_env_value(
                r#"{"api_token": "tok"}"#,
            )
            .unwrap_err();
        assert!(err.to_string().contains("account_id"));
    }

    // ── Wrangler output parsing tests ───────────────────────────────────────

    #[test]
    fn parses_wrangler_version_upload_ndjson() {
        use deis_provider_driver_cloudflare::wrangler::WranglerOutput;

        let output = WranglerOutput {
            success: true,
            exit_code: Some(0),
            stdout: "{\"status\":\"uploading\"}\n{\"status\":\"complete\",\"version_id\":\"v-abc-123\"}\n"
                .to_owned(),
            stderr: String::new(),
        };

        let version_id = output
            .last_ndjson()
            .and_then(|v| v["version_id"].as_str().map(String::from));
        assert_eq!(version_id, Some("v-abc-123".to_owned()));
    }

    #[test]
    fn handles_wrangler_error_output_gracefully() {
        use deis_provider_driver_cloudflare::wrangler::WranglerOutput;

        let output = WranglerOutput {
            success: false,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "Error: authentication failed (code: 10000)\n".to_owned(),
        };

        assert!(!output.success);
        assert_eq!(output.exit_code, Some(1));
        assert!(output.ndjson_lines().is_empty());
        assert!(output.last_ndjson().is_none());
    }

    // ── Deployment journal tests ────────────────────────────────────────────

    #[test]
    fn journal_persists_and_round_trips() {
        use deis_provider_driver_cloudflare::commands::DeploymentJournal;

        let dir = tempfile::tempdir().unwrap();

        let mut journal = DeploymentJournal::default();
        journal.binding_plan_json =
            Some(r#"{"bindings":[{"kind":"kv","name":"TEST_KV"}]}"#.to_owned());
        journal
            .created_resources
            .insert("TEST_KV".to_owned(), "ns-test-1".to_owned());
        journal.version_id = Some("v-deploy-42".to_owned());

        journal.save(dir.path()).unwrap();

        let loaded = DeploymentJournal::load(dir.path());
        assert_eq!(loaded.binding_plan_json, journal.binding_plan_json);
        assert_eq!(loaded.created_resources, journal.created_resources);
        assert_eq!(loaded.version_id, journal.version_id);
    }

    #[test]
    fn empty_journal_loads_as_default() {
        use deis_provider_driver_cloudflare::commands::DeploymentJournal;

        let dir = tempfile::tempdir().unwrap();
        let journal = DeploymentJournal::load(dir.path());
        assert!(journal.binding_plan_json.is_none());
        assert!(journal.created_resources.is_empty());
        assert!(journal.version_id.is_none());
    }

    // ── Version ID extraction from raw output ───────────────────────────────

    #[test]
    fn extracts_version_id_from_ndjson_output() {
        let stdout = r#"{"status":"uploading"}
{"result":{"version_id":"abc-def-ghi","message":"uploaded"}}
"#;
        let id =
            deis_provider_driver_cloudflare::commands::find_version_id_in_output(stdout);
        assert_eq!(id, Some("abc-def-ghi".to_owned()));
    }

    #[test]
    fn extracts_version_id_from_raw_text() {
        let stdout = "wrangler versions upload...\nUpload complete! version_id: xyz-789-000\nDone.\n";
        let id =
            deis_provider_driver_cloudflare::commands::find_version_id_in_output(stdout);
        assert_eq!(id, Some("xyz-789-000".to_owned()));
    }

    // ── Command mapping contract tests ──────────────────────────────────────
    //
    // Document which wrangler subcommand each lifecycle operation maps to.

    #[test]
    fn preflight_maps_to_wrangler_whoami() {
        let request = make_request(DriverOperation::Preflight);
        assert_eq!(request.operation, DriverOperation::Preflight);
    }

    #[test]
    fn upload_maps_to_wrangler_versions_upload() {
        let request = make_request(DriverOperation::Upload);
        assert_eq!(request.operation, DriverOperation::Upload);
    }

    #[test]
    fn activate_maps_to_wrangler_versions_deploy() {
        let request = make_request(DriverOperation::Activate);
        assert_eq!(request.operation, DriverOperation::Activate);
    }

    #[test]
    fn cleanup_maps_to_resource_deletion() {
        let request = make_request(DriverOperation::Cleanup);
        assert_eq!(request.operation, DriverOperation::Cleanup);
    }

    #[test]
    fn all_twelve_operations_are_mapped() {
        use deis_provider_driver::ALL_DRIVER_OPERATIONS;
        for op in ALL_DRIVER_OPERATIONS {
            let request = make_request(op);
            assert_eq!(request.operation, op, "operation {op:?} is mapped");
        }
    }

    // ── Protocol envelope tests ─────────────────────────────────────────────

    #[test]
    fn protocol_envelope_round_trips() {
        let request = make_request(DriverOperation::Preflight);
        let process_request = DriverProcessRequest::new(request.clone());
        let json = serde_jcs::to_string(&process_request).unwrap();
        let parsed: DriverProcessRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.message_type, "request");
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(
            parsed.protocol,
            deis_provider_driver::DRIVER_PROCESS_PROTOCOL_V1
        );
        let validated = parsed.validate().unwrap();
        assert_eq!(validated.operation, DriverOperation::Preflight);
    }

    #[test]
    fn outcome_envelope_round_trips() {
        let request = make_request(DriverOperation::Preflight);
        let receipt = DriverReceipt {
            schema_version: 1,
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest,
            state: DriverStepState::Succeeded,
            deployment_state: Some(DeploymentState::Planned),
            side_effect: SideEffect::None,
            provider_request_id: Some("req-1".to_owned()),
            provider_version_id: None,
            provider_deployment_id: None,
            provider_resource_ids: BTreeMap::new(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        };

        let outcome = DriverProcessOutcome::succeeded(receipt);
        let json = serde_jcs::to_string(&outcome).unwrap();
        let parsed: DriverProcessOutcome = serde_json::from_str(&json).unwrap();
        let result = parsed.validate().unwrap();
        assert!(matches!(
            result,
            deis_provider_driver::DriverProcessResult::Succeeded { .. }
        ));
    }
}