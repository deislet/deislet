#!/usr/bin/env python3
"""Check Deislet's ADR, standard lock, conformance and roadmap invariants."""

from __future__ import annotations

import hashlib
import json
import re
import sys
import tomllib
from pathlib import Path

import jsonschema


ROOT = Path(__file__).resolve().parents[1]
EXPECTED_ADRS = list(range(1, 25))
EXPECTED_PHASES = [f"phase-{number}" for number in range(6)]
GIT_SHA = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
WORK_ID = re.compile(r"^P([0-5])-[A-Z]+-[0-9]{3}$")
EXPECTED_WORKSPACE_TIMEOUT_MINUTES = 60
EXPECTED_TEST_SHARD_TIMEOUT_MINUTES = 90
EXPECTED_TEST_SHARDS = {
    "runtime-tests": "cargo test -p deis-runtime --no-fail-fast",
    "compiler-cli-tests": (
        "cargo test -p deis-bundle -p deis-node-contract -p deis-build-core "
        "-p deis-build-adapter-cloudflare -p deis-build-adapter-deislet "
        "-p deis-build-adapter-deno -p deis-build-adapter-tencent "
        "-p deis-build -p deis --no-fail-fast"
    ),
    "control-services-tests": (
        "cargo test -p deis-wire -p deis-backend-migration -p deis-backend-topology "
        "-p deis-control -p deis-store "
        "-p deis-queue -p deis-telemetry -p deis-proxy -p deislet-integration-tests "
        "--no-fail-fast"
    ),
}
EXPECTED_TEST_PREREQUISITES = {
    "runtime-tests": "cargo build -p deis-proxy",
}


class ValidationError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValidationError(message)


def read_text(relative: str) -> str:
    path = ROOT / relative
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        raise ValidationError(f"{relative}: {error}") from error


def load_json(relative: str) -> dict:
    try:
        value = json.loads(read_text(relative))
    except json.JSONDecodeError as error:
        raise ValidationError(f"{relative}: {error}") from error
    require(isinstance(value, dict), f"{relative}: top-level value must be an object")
    return value


def workspace_package_names() -> set[str]:
    workspace = tomllib.loads(read_text("Cargo.toml"))
    names: set[str] = set()
    for member in workspace["workspace"]["members"]:
        for manifest in ROOT.glob(f"{member}/Cargo.toml"):
            document = tomllib.loads(manifest.read_text(encoding="utf-8"))
            names.add(document["package"]["name"])
    return names


def validate_adrs() -> None:
    adr_dir = ROOT / "docs/adr"
    files = sorted(adr_dir.glob("[0-9][0-9][0-9]-*.md"))
    numbers = [int(path.name[:3]) for path in files]
    require(
        numbers == EXPECTED_ADRS,
        f"ADR set must be exactly 001..{EXPECTED_ADRS[-1]:03d}, got {numbers}",
    )

    for path, number in zip(files, EXPECTED_ADRS, strict=True):
        text = path.read_text(encoding="utf-8")
        relative = path.relative_to(ROOT)
        require(text.startswith(f"# ADR-{number:03d}："), f"{relative}: title number mismatch")
        require("- 状态：Accepted" in text[:400], f"{relative}: status is not Accepted")
        require(
            "../product/phase-analysis-2026-09-03.zh.md" in text[:500],
            f"{relative}: phase-analysis relation is missing",
        )
        require("Edge Canon Proposal 0001" in text[:900], f"{relative}: upstream proposal link is missing")
        require("/tmp/" not in text, f"{relative}: temporary path remains")

    index = read_text("docs/adr/README.md")
    for number in EXPECTED_ADRS:
        require(f"| [{number:03d}](" in index, f"ADR index is missing {number:03d}")
    adr_005 = read_text("docs/adr/005-portable-guarantees.md")
    adr_022 = read_text("docs/adr/022-entry-plan-resource-baseline.md")
    require("ADR-022" in adr_005, "ADR-005 must point at its superseding resource baseline decision")
    require("ADR-005" in adr_022, "ADR-022 must point back at the decision it partially supersedes")

    analysis = read_text("docs/product/phase-analysis-2026-09-03.zh.md")
    require("/tmp/" not in analysis, "phase analysis still references /tmp")
    require("已归档的阶段分析" in analysis, "phase analysis archival status is missing")


def validate_standard_lock() -> str:
    lock = load_json("standards/edge-canon.lock.json")
    require(lock.get("schemaVersion") == 1, "standard lock schemaVersion must be 1")
    source = lock.get("source", {})
    commit = source.get("sourceCommit", "")
    require(bool(GIT_SHA.fullmatch(commit)), "standard lock sourceCommit must be a full Git SHA")
    standard = lock.get("standard", {})
    require(standard.get("contractId") == "edge-canon.next", "unexpected locked contractId")
    require(standard.get("releaseStatus") == "proposal", "phase 0 must not present next as released")
    require(standard.get("normativeRelease") is False, "proposal cannot be normative")
    require(standard.get("capabilityModel") == "single-standard", "only the single standard model is valid")
    require(standard.get("profiles") == [], "standard lock must not contain profiles")
    resource_guarantees = standard.get("resourceGuarantees", {})
    require(
        resource_guarantees.get("referencePlanBaseline")
        == "lowest-generally-available-free-or-entry-plan",
        "standard lock must pin the lowest free/entry plan resource baseline",
    )
    require(
        resource_guarantees.get("allReferenceVendorsMustMeetBaseline") is True,
        "standard lock must require every reference vendor to meet the baseline",
    )
    require(
        resource_guarantees.get("unknownPublicGuarantee") == "release-blocker",
        "unknown public resource guarantees must block standard release",
    )
    require(
        resource_guarantees.get("higherPlanCapacityIsPortable") is False,
        "higher provider plan capacity must not be portable",
    )
    expected_sources = {
        "proposalPath": "proposals/0001-unified-platform-contract/README.zh.md",
        "webFetchEventsFixturePath": "conformance/harness/web-fetch-events/fixture.mjs",
        "webFetchEventsCpuWorkloadPath": "conformance/harness/web-fetch-events/cpu-workload.mjs",
        "contractPath": "standard/contract.json",
        "contractSchemaPath": "schemas/standard-contract.schema.json",
        "requirementsPath": "standard/requirements.json",
        "requirementsSchemaPath": "schemas/requirements-registry.schema.json",
        "canonicalArtifactRequirementsPath": "standard/requirements/canonical-build-artifact.zh.md",
        "canonicalArtifactSchemaPath": "schemas/canonical-build-output.schema.json",
        "canonicalArtifactHarnessPath": "conformance/harness/canonical-build-artifact/harness.json",
        "canonicalArtifactPlatformEvidencePath": "conformance/evidence/canonical-build-artifact-platforms-2026-09-04.json",
        "routingRequirementsPath": "standard/requirements/routing-static-assets.zh.md",
        "routingSchemaPath": "schemas/routing-static-assets.schema.json",
        "routingHarnessPath": "conformance/harness/routing-static-assets/harness.json",
        "routingPlatformEvidencePath": "conformance/evidence/routing-static-assets-platforms-2026-09-04.json",
        "streamsRequirementsPath": "standard/requirements/streams-websockets-background-work.zh.md",
        "streamsSchemaPath": "schemas/streams-websockets-background-work.schema.json",
        "streamsHarnessPath": "conformance/harness/streams-websockets-background-work/harness.json",
        "streamsPlatformEvidencePath": "conformance/evidence/streams-websockets-background-work-platforms-2026-09-04.json",
        "streamsProviderRuntimeProtocolPath": "conformance/harness/streams-websockets-background-work/provider-runtime-protocol.zh.md",
        "streamsProviderRuntimeFixturePath": "conformance/harness/streams-websockets-background-work/provider-runtime-fixture.mjs",
        "streamsProviderRuntimeCollectorPath": "conformance/harness/streams-websockets-background-work/provider-runtime-collect.mjs",
        "streamsProviderRuntimeOraclePath": "conformance/harness/streams-websockets-background-work/provider-runtime-oracle.mjs",
        "nodeNpmRequirementsPath": "standard/requirements/node-npm-subset.zh.md",
        "nodeNpmSchemaPath": "schemas/node-npm-subset.schema.json",
        "nodeNpmHarnessPath": "conformance/harness/node-npm-subset/harness.json",
        "nodeNpmPlatformEvidencePath": "conformance/evidence/node-npm-subset-platforms-2026-09-04.json",
        "webPlatformApisRequirementsPath": "standard/requirements/web-platform-apis.zh.md",
        "webPlatformApisSchemaPath": "schemas/web-platform-apis.schema.json",
        "webPlatformApisHarnessPath": "conformance/harness/web-platform-apis/harness.json",
        "webPlatformApisPlatformEvidencePath": "conformance/evidence/web-platform-apis-platforms-2026-09-04.json",
        "conformanceKitPath": "conformance/kit.json",
        "conformanceKitSchemaPath": "schemas/conformance-kit.schema.json",
        "registryPath": "conformance/registry.json",
        "registrySchemaPath": "schemas/conformance-registry.schema.json",
        "registrySnapshotPath": "standards/edge-canon.conformance-registry.json",
    }
    for key, expected in expected_sources.items():
        require(source.get(key) == expected, f"standard lock {key} must be {expected}")
    for source_key, local_name in (
        ("webFetchEventsFixtureSha256", "edge-canon-web-fetch-events.mjs"),
        ("webFetchEventsCpuWorkloadSha256", "edge-canon-web-fetch-events-cpu-workload.mjs"),
    ):
        digest = source.get(source_key, "")
        require(bool(SHA256.fullmatch(digest)), f"standard lock {source_key} is not SHA-256")
        local_fixture = ROOT / "standards" / "fixtures" / local_name
        require(local_fixture.is_file(), f"vendored fixture is missing: {local_name}")
        require(
            hashlib.sha256(local_fixture.read_bytes()).hexdigest() == digest,
            f"vendored fixture differs from the standard lock: {local_name}",
        )
    require(
        bool(SHA256.fullmatch(source.get("canonicalArtifactSchemaSha256", ""))),
        "standard lock canonical artifact schema digest is not SHA-256",
    )
    require(
        bool(SHA256.fullmatch(source.get("routingSchemaSha256", ""))),
        "standard lock routing schema digest is not SHA-256",
    )
    require(
        bool(SHA256.fullmatch(source.get("streamsSchemaSha256", ""))),
        "standard lock streams schema digest is not SHA-256",
    )
    streams_fixture_digest = source.get("streamsProviderRuntimeFixtureSha256", "")
    require(
        bool(SHA256.fullmatch(streams_fixture_digest)),
        "standard lock streams provider runtime fixture digest is not SHA-256",
    )
    local_streams_fixture = ROOT / "standards/fixtures/edge-canon-stream-provider-runtime.mjs"
    require(local_streams_fixture.is_file(), "vendored streams provider runtime fixture is missing")
    require(
        hashlib.sha256(local_streams_fixture.read_bytes()).hexdigest() == streams_fixture_digest,
        "vendored streams provider runtime fixture differs from the standard lock",
    )
    require(
        bool(SHA256.fullmatch(source.get("nodeNpmSchemaSha256", ""))),
        "standard lock Node/npm schema digest is not SHA-256",
    )
    require(
        bool(SHA256.fullmatch(source.get("webPlatformApisSchemaSha256", ""))),
        "standard lock Web Platform APIs schema digest is not SHA-256",
    )
    verification = lock.get("verification", {})
    require(verification.get("upstreamResult") == "passed", "locked upstream governance check did not pass")
    require(verification.get("expectedCapabilityFamilies") == 15, "unexpected capability family count")
    require(verification.get("expectedDraftClauses") == 151, "unexpected draft clause count")
    require(verification.get("expectedConformanceSuites") == 15, "unexpected conformance suite count")
    require(verification.get("expectedBackends") == 4, "unexpected backend record count")

    registry_digest = source.get("registrySha256", "")
    require(bool(SHA256.fullmatch(registry_digest)), "standard lock registrySha256 is not SHA-256")
    registry_relative = source["registrySnapshotPath"]
    try:
        registry_bytes = (ROOT / registry_relative).read_bytes()
    except OSError as error:
        raise ValidationError(f"{registry_relative}: {error}") from error
    actual_registry_digest = hashlib.sha256(registry_bytes).hexdigest()
    require(
        actual_registry_digest == registry_digest,
        f"{registry_relative}: digest differs from the standard lock",
    )
    registry = load_json(registry_relative)
    require(registry.get("standardId") == standard.get("contractId"), "registry standardId drifted")
    require(
        registry.get("standardReleaseStatus") == standard.get("releaseStatus"),
        "registry release status drifted",
    )

    pinned_documents = [
        "docs/adr/README.md",
        "docs/product/README.md",
        "docs/product/roadmap.json",
        *[str(path.relative_to(ROOT)) for path in sorted((ROOT / "docs/adr").glob("[0-9][0-9][0-9]-*.md"))],
    ]
    for relative in pinned_documents:
        require(commit in read_text(relative), f"{relative}: does not pin the locked Edge Canon commit")
    return commit


def validate_roadmap(locked_standard_commit: str) -> None:
    load_json("docs/product/roadmap.schema.json")
    roadmap = load_json("docs/product/roadmap.json")
    require(roadmap.get("schemaVersion") == 1, "roadmap schemaVersion must be 1")
    require(roadmap.get("status") == "active", "productization goal must remain active")

    for relative in roadmap.get("authorities", {}).values():
        require((ROOT / relative).is_file(), f"roadmap authority is missing: {relative}")

    phases = roadmap.get("phases", [])
    phase_ids = [phase.get("id") for phase in phases]
    require(phase_ids == EXPECTED_PHASES, f"roadmap phases must be ordered {EXPECTED_PHASES}")
    require(phases[0].get("status") == "active", "phase 0 must be active")
    require(all(phase.get("status") == "queued" for phase in phases[1:]), "later phases must remain queued")
    require(
        any(
            f"{len(EXPECTED_ADRS)} 份 Accepted ADR" in criterion
            for criterion in phases[0].get("exitCriteria", [])
        ),
        "phase 0 exit criteria must state the actual accepted ADR count",
    )

    for index, phase in enumerate(phases):
        expected_dependencies = [] if index == 0 else [f"phase-{index - 1}"]
        require(
            phase.get("dependsOn") == expected_dependencies,
            f"{phase['id']}: must depend only on the preceding phase",
        )

    work: dict[str, dict] = {}
    for phase_number, phase in enumerate(phases):
        require(phase.get("exitCriteria"), f"{phase['id']}: exit criteria are empty")
        for item in phase.get("workItems", []):
            item_id = item.get("id", "")
            match = WORK_ID.fullmatch(item_id)
            require(match is not None, f"invalid work item ID: {item_id}")
            require(int(match.group(1)) == phase_number, f"{item_id}: phase prefix mismatch")
            require(item_id not in work, f"duplicate work item ID: {item_id}")
            work[item_id] = item

    for item_id, item in work.items():
        for dependency in item.get("dependsOn", []):
            require(dependency in work, f"{item_id}: unknown dependency {dependency}")
            require(dependency != item_id, f"{item_id}: self dependency")
        if item.get("status") == "done":
            for output in item.get("outputs", []):
                output_type = output.get("type")
                value = output.get("value", "")
                require(output_type != "planned-path", f"{item_id}: done item has a planned output")
                if output_type == "local-path":
                    require((ROOT / value).is_file(), f"{item_id}: completed output is missing: {value}")
                elif output_type == "url":
                    require(value.startswith("https://"), f"{item_id}: invalid output URL")

    for item_id, item in work.items():
        if item.get("status") == "done":
            unfinished = [dep for dep in item.get("dependsOn", []) if work[dep].get("status") != "done"]
            require(not unfinished, f"{item_id}: done before dependencies {unfinished}")

    next_actions = roadmap.get("nextActions", [])
    require(next_actions, "nextActions must not be empty while roadmap is active")
    for item_id in next_actions:
        require(item_id in work, f"nextActions references unknown item {item_id}")
        require(work[item_id].get("status") in {"active", "queued"}, f"{item_id}: invalid next-action status")
    require(work[next_actions[0]].get("status") == "active", "first next action must be active")
    spec = work.get("P0-SPEC-001", {})
    require(
        locked_standard_commit in spec.get("verification", ""),
        "P0-SPEC-001 verification must name the currently locked Edge Canon commit",
    )

    status_schema = load_json("docs/product/target-status.schema.json")
    target_status = load_json("docs/product/target-status.json")
    try:
        jsonschema.validate(
            instance=target_status,
            schema=status_schema,
            format_checker=jsonschema.FormatChecker(),
        )
    except jsonschema.ValidationError as error:
        location = "/".join(str(part) for part in error.absolute_path) or "<root>"
        raise ValidationError(f"target-status.json {location}: {error.message}") from error


def validate_integration() -> None:
    workflow = read_text(".github/workflows/checks.yml")
    require(
        "python3 scripts/check-product-governance.py" in workflow,
        "CI does not run the product governance check",
    )
    require(
        "python3 scripts/check-shared-contracts.py" in workflow,
        "CI does not execute the shared product contract fixtures",
    )
    require(
        "python3 scripts/generate-target-status.py --check" in workflow,
        "CI does not reject registry-derived target status drift",
    )
    require((ROOT / "contracts/v1/registry.json").is_file(), "shared product contract registry is missing")
    require((ROOT / "contracts/v1/README.zh.md").is_file(), "shared product contract documentation is missing")
    require("cargo audit" in workflow, "CI dependency audit is missing")
    workspace_job = re.search(
        r"(?ms)^  workspace:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)",
        workflow,
    )
    require(workspace_job is not None, "CI workspace job is missing")
    timeout = re.search(r"(?m)^    timeout-minutes:\s*([0-9]+)\s*$", workspace_job.group("body"))
    require(timeout is not None, "CI workspace job must have a finite timeout")
    require(
        int(timeout.group(1)) == EXPECTED_WORKSPACE_TIMEOUT_MINUTES,
        "CI static workspace gates must remain at the measured "
        f"{EXPECTED_WORKSPACE_TIMEOUT_MINUTES}-minute ceiling",
    )
    require(
        "cargo test --workspace" not in workspace_job.group("body"),
        "CI must not put every workspace test back behind one opaque job ceiling",
    )
    for job_id, command in EXPECTED_TEST_SHARDS.items():
        shard = re.search(
            rf"(?ms)^  {re.escape(job_id)}:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)",
            workflow,
        )
        require(shard is not None, f"CI test shard {job_id} is missing")
        shard_timeout = re.search(
            r"(?m)^    timeout-minutes:\s*([0-9]+)\s*$", shard.group("body")
        )
        require(shard_timeout is not None, f"CI test shard {job_id} must have a finite timeout")
        require(
            int(shard_timeout.group(1)) == EXPECTED_TEST_SHARD_TIMEOUT_MINUTES,
            f"CI test shard {job_id} must keep the "
            f"{EXPECTED_TEST_SHARD_TIMEOUT_MINUTES}-minute ceiling",
        )
        require(command in shard.group("body"), f"CI test shard {job_id} command drifted")
        prerequisite = EXPECTED_TEST_PREREQUISITES.get(job_id)
        if prerequisite is not None:
            require(
                prerequisite in shard.group("body"),
                f"CI test shard {job_id} lacks required integration-test prerequisite {prerequisite}",
            )
    assigned_packages = [
        package
        for command in EXPECTED_TEST_SHARDS.values()
        for package in re.findall(r"(?:^|\s)-p\s+([a-zA-Z0-9_-]+)", command)
    ]
    require(
        len(assigned_packages) == len(set(assigned_packages)),
        "CI test shards assign at least one workspace package more than once",
    )
    packages = workspace_package_names()
    require(
        set(assigned_packages) == packages,
        "CI test shards must cover every workspace package exactly once: "
        f"expected {sorted(packages)}, got {sorted(assigned_packages)}",
    )
    # These are the first official-action majors that run natively on Node
    # 24. Keep the set explicit: silently falling back to an older major
    # reintroduces the runner compatibility mode and its deprecation warning;
    # adding another official action requires recording its reviewed major.
    expected_official_actions = {
        "actions/checkout@v5",
        "actions/setup-node@v5",
        "actions/setup-python@v6",
        "actions/cache@v5",
        "actions/upload-artifact@v6",
    }
    actual_official_actions = set(re.findall(r"uses:\s+(actions/[a-z-]+@v[0-9]+)", workflow))
    require(
        actual_official_actions == expected_official_actions,
        "official GitHub Actions must stay on the reviewed Node 24 majors: "
        f"expected {sorted(expected_official_actions)}, got {sorted(actual_official_actions)}",
    )
    audit_policy = read_text(".cargo/audit.toml")
    expected_advisories = {
        "RUSTSEC-2026-0119",
        "RUSTSEC-2026-0118",
        "RUSTSEC-2024-0437",
        "RUSTSEC-2023-0071",
    }
    recorded_advisories = set(re.findall(r'"(RUSTSEC-[0-9]{4}-[0-9]{4})"', audit_policy))
    require(
        recorded_advisories == expected_advisories,
        "cargo audit exceptions must remain the four explicitly reviewed advisories",
    )
    implementation = read_text("docs/implementation-plan.zh.md")
    require("文档边界（2026-09-03）" in implementation, "legacy implementation spec lacks its authority boundary")
    product = read_text("docs/product/README.md")
    require(
        "target-status.zh.md" in product and "target-status.json" in product,
        "product entry does not delegate support status to generated registry views",
    )
    for path in (
        "README.md",
        "crates/deis-build/README.md",
        "crates/deis-build/README.zh.md",
    ):
        document = read_text(path)
        require(
            "target-status" in document,
            f"{path} does not delegate current target support status to the registry view",
        )
    cli_targets = read_text("crates/deis/src/commands/targets.rs")
    web_targets = read_text("crates/deis-control/web/src/pages/Targets.tsx")
    require(
        'include_str!("target-status.json")' in cli_targets,
        "CLI target status is not generated-registry-backed",
    )
    require(
        '"generatedFrom": raw["generatedFrom"]' in cli_targets
        and '"standard": raw["standard"]' in cli_targets,
        "CLI target inspection drops registry or standard provenance",
    )
    require(
        "@/generated/target-status.json" in web_targets,
        "Web target status is not generated-registry-backed",
    )
    compiler = read_text("crates/deis-build/src/compiler/mod.rs")
    coverage = read_text("crates/deis-build-core/src/target_artifact_coverage.rs")
    require(
        "PlatformCapabilities" not in compiler + coverage
        and "UnsupportedFeatureError" not in compiler + coverage
        and "alternatives(" not in compiler + coverage,
        "compiler still exposes target artifact coverage as platform support",
    )
    phantom_features = (
        "Feature::SSR",
        "Feature::ISR",
        "Feature::WebSockets",
        "Feature::DurableObjects",
        "Feature::WorkersAI",
        "Feature::BroadcastChannel",
    )
    require(
        not any(feature in coverage for feature in phantom_features),
        "target artifact table contains APIs the compiler cannot detect from project declarations",
    )
    stale_build_claims = (
        "Platform capability validation failed",
        "Feature 'SQL Database' is not supported on EdgeOne Pages",
        "No platform compatibility check needed",
        "Basic services (KV, Cache) are always available",
    )
    for path in (
        "crates/deis-build/docs/basic-services.md",
        "crates/deis-build/docs/basic-services.zh.md",
    ):
        document = read_text(path)
        require(
            "target-status" in document
            and not any(claim in document for claim in stale_build_claims),
            f"{path} contains a hand-maintained support claim or obsolete compiler output",
        )
    adapter_index = read_text("crates/deis-build/docs/adapter-specs.zh.md")
    require(
        "target-status" in adapter_index
        and "## Comparison Matrix" not in adapter_index
        and "authoritative reference" not in adapter_index,
        "adapter research index still presents unpinned notes as conformance or support",
    )
    for path in (
        "crates/deis-build-adapter-cloudflare/spec.mdc",
        "crates/deis-build-adapter-deno/spec.mdc",
        "crates/deis-build-adapter-tencent/spec.mdc",
    ):
        require(
            "Boundary:" in read_text(path),
            f"{path} lacks a visible non-normative research boundary",
        )
    for vendor in ("cloudflare", "deno", "tencent"):
        artifact_test = ROOT / f"crates/deis-build-adapter-{vendor}/tests/artifact_shape.rs"
        require(
            artifact_test.is_file(),
            f"{artifact_test.relative_to(ROOT)} is missing; adapter artifact checks must not be named conformance",
        )
    contributing = read_text("crates/deis/CONTRIBUTING.md")
    require(
        "artifact_shape" in contributing
        and "spec_compliance" not in contributing
        and "An adapter existing or producing an accepted artifact is not support" in contributing,
        "target contribution guide conflates artifact generation with conformance or support",
    )


def main() -> int:
    try:
        validate_adrs()
        commit = validate_standard_lock()
        validate_roadmap(commit)
        validate_integration()
    except (KeyError, TypeError, ValidationError) as error:
        print(f"product governance validation failed: {error}", file=sys.stderr)
        return 1

    print(
        f"product governance validation passed: {len(EXPECTED_ADRS)} ADRs, 6 roadmap phases, "
        f"Edge Canon {commit[:7]} locked, no temporary paths"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
