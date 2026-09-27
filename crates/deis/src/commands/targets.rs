//! `deis targets` - the evidence and certification state of every build target.
//!
//! The bytes below are generated from the SHA-256-locked Edge Canon
//! conformance registry by `scripts/generate-target-status.py`. They are not a
//! second capability table: the same generated JSON drives the product docs
//! and the control-plane Web console, and CI refuses any drift between them.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::commands::escaped;
use crate::render::{Output, Table};

const STATUS_JSON: &str = include_str!("target-status.json");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetStatus {
    generated_from: GeneratedFrom,
    standard: Standard,
    backends: Vec<Backend>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedFrom {
    source_commit: String,
    registry_sha256: String,
    registry_updated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Standard {
    id: String,
    version: String,
    release_status: String,
    normative_release: bool,
    certification_unit: String,
    required_maturity: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Backend {
    id: String,
    display_name: String,
    role: String,
    lifecycle: String,
    implementation: Implementation,
    evidence: Evidence,
    certification: Certification,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Implementation {
    repository: String,
    source_commit: String,
    scope: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Evidence {
    maturity: String,
    observed_at: String,
    outcome: String,
    records: Vec<EvidenceRecord>,
}

#[derive(Debug, Deserialize)]
struct EvidenceRecord {
    kind: String,
    reference: String,
    note: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Certification {
    status: String,
    compliant: bool,
    supported: bool,
    valid_until: Option<String>,
    blockers: Vec<String>,
}

fn document() -> Result<(serde_json::Value, TargetStatus)> {
    let raw: serde_json::Value = serde_json::from_str(STATUS_JSON)
        .context("the embedded registry-derived target status is not JSON")?;
    let status = serde_json::from_value(raw.clone())
        .context("the embedded registry-derived target status has an invalid shape")?;
    Ok((raw, status))
}

pub fn list(output: Output) -> Result<()> {
    let (raw, status) = document()?;
    if output.json {
        return output.emit(&raw);
    }

    println!(
        "Edge Canon {} ({}) — {}{}",
        escaped(&status.standard.id),
        escaped(&status.standard.version),
        escaped(&status.standard.release_status),
        if status.standard.normative_release {
            ""
        } else {
            ", not a normative release"
        }
    );
    println!(
        "Certification unit: {}; required evidence: {}",
        escaped(&status.standard.certification_unit),
        escaped(&status.standard.required_maturity)
    );
    println!(
        "Registry: {} at {} (SHA-256 {})",
        short(&status.generated_from.source_commit),
        escaped(&status.generated_from.registry_updated_at),
        short(&status.generated_from.registry_sha256)
    );
    println!();

    let mut table = Table::new(&[
        "TARGET",
        "ROLE",
        "LIFECYCLE",
        "EVIDENCE",
        "RESULT",
        "CERTIFICATION",
        "COMPLIANT",
        "SUPPORTED",
    ]);
    for backend in &status.backends {
        table.row(vec![
            format!(
                "{} ({})",
                escaped(&backend.display_name),
                escaped(&backend.id)
            ),
            escaped(&backend.role),
            escaped(&backend.lifecycle),
            escaped(&backend.evidence.maturity),
            escaped(&backend.evidence.outcome),
            escaped(&backend.certification.status),
            yes_no(backend.certification.compliant).to_string(),
            yes_no(backend.certification.supported).to_string(),
        ]);
    }
    table.print();
    Ok(())
}

pub fn inspect(id: &str, output: Output) -> Result<()> {
    let (raw, status) = document()?;
    let Some((index, backend)) = status
        .backends
        .iter()
        .enumerate()
        .find(|(_, backend)| backend.id == id)
    else {
        let choices = status
            .backends
            .iter()
            .map(|backend| backend.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "unknown target {}; choose one of: {}",
            escaped(id),
            escaped(&choices)
        );
    };

    if output.json {
        let inspected = serde_json::json!({
            "generatedFrom": raw["generatedFrom"].clone(),
            "standard": raw["standard"].clone(),
            "target": raw["backends"][index].clone(),
        });
        return output.emit(&inspected);
    }

    println!(
        "Edge Canon {} {} ({})",
        escaped(&status.standard.id),
        escaped(&status.standard.version),
        escaped(&status.standard.release_status)
    );
    println!(
        "Registry SHA-256: {}",
        escaped(&status.generated_from.registry_sha256)
    );
    println!(
        "{} ({})",
        escaped(&backend.display_name),
        escaped(&backend.id)
    );
    println!("Role: {}", escaped(&backend.role));
    println!(
        "Lifecycle: {}{}",
        escaped(&backend.lifecycle),
        if backend.lifecycle == "frozen" {
            " (stateless function artifacts only; no bindings, no certification)"
        } else {
            ""
        }
    );
    println!(
        "Implementation evidence: {} @ {}",
        escaped(&backend.implementation.repository),
        short(&backend.implementation.source_commit)
    );
    println!("Scope: {}", escaped(&backend.implementation.scope));
    println!(
        "Evidence: {} / {} (observed {})",
        escaped(&backend.evidence.maturity),
        escaped(&backend.evidence.outcome),
        escaped(&backend.evidence.observed_at)
    );
    println!(
        "Certification: {}; compliant: {}; supported: {}; valid until: {}",
        escaped(&backend.certification.status),
        yes_no(backend.certification.compliant),
        yes_no(backend.certification.supported),
        backend
            .certification
            .valid_until
            .as_deref()
            .map(escaped)
            .unwrap_or_else(|| "none".to_string())
    );

    println!("\nEvidence records:");
    for record in &backend.evidence.records {
        println!(
            "  - {}: {}\n    {}",
            escaped(&record.kind),
            escaped(&record.reference),
            escaped(&record.note)
        );
    }
    println!("\nCertification blockers:");
    if backend.certification.blockers.is_empty() {
        println!("  - none");
    } else {
        for blocker in &backend.certification.blockers {
            println!("  - {}", escaped(blocker));
        }
    }
    Ok(())
}

fn short(value: &str) -> String {
    escaped(&value.chars().take(12).collect::<String>())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_status_is_the_locked_proposal_and_has_unique_targets() {
        let (_, status) = document().unwrap();
        assert_eq!(status.standard.id, "edge-canon.next");
        assert!(!status.standard.normative_release);
        let mut ids = status
            .backends
            .iter()
            .map(|backend| backend.id.as_str())
            .collect::<Vec<_>>();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before);
    }

    #[test]
    fn frozen_targets_promise_nothing() {
        let (_, status) = document().unwrap();
        let frozen: Vec<&str> = status
            .backends
            .iter()
            .filter(|backend| backend.lifecycle == "frozen")
            .map(|backend| backend.id.as_str())
            .collect();
        assert_eq!(frozen, vec!["tencent-edgeone-makers", "deno"]);
        assert!(
            status
                .backends
                .iter()
                .all(|backend| backend.lifecycle != "frozen"
                    || (!backend.certification.compliant && !backend.certification.supported))
        );
    }

    #[test]
    fn proposal_never_surfaces_as_compliant_or_supported() {
        let (_, status) = document().unwrap();
        assert!(
            status
                .backends
                .iter()
                .all(|backend| !backend.certification.compliant
                    && !backend.certification.supported)
        );
    }

    #[test]
    fn inspect_projection_keeps_standard_and_registry_provenance() {
        let (raw, status) = document().unwrap();
        let backend = &status.backends[0];
        let index = status
            .backends
            .iter()
            .position(|candidate| candidate.id == backend.id)
            .unwrap();
        let inspected = serde_json::json!({
            "generatedFrom": raw["generatedFrom"].clone(),
            "standard": raw["standard"].clone(),
            "target": raw["backends"][index].clone(),
        });

        assert_eq!(
            inspected["generatedFrom"]["registrySha256"],
            status.generated_from.registry_sha256
        );
        assert_eq!(inspected["standard"]["id"], status.standard.id);
        assert_eq!(inspected["target"]["id"], backend.id);
    }
}
