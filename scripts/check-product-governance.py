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
EXPECTED_PHASES = [f"phase-{number}" for number in range(6)]
GIT_SHA = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
WORK_ID = re.compile(r"^P([0-5])-[A-Z]+-[0-9]{3}$")
EXPECTED_WORKSPACE_TIMEOUT_MINUTES = 60
EXPECTED_TEST_SHARD_TIMEOUT_MINUTES = 90
EXPECTED_TEST_SHARDS = {
    "runtime-tests": "cargo test -p deis-runtime --no-fail-fast",
    "compiler-cli-tests": (
        "cargo test -p deis-bundle -p deis-node-contract -p deis-artifact -p deis-build-core "
        "-p deis-build-adapter-cloudflare -p deis-build-adapter-deislet "
        "-p deis-build-adapter-deno -p deis-build-adapter-tencent "
        "-p deis-build -p deis --no-fail-fast"
    ),
    "control-services-tests": (
        "cargo test -p deis-wire -p deis-identity -p deis-identity-test-pki "
        "-p deis-provider-driver -p deis-backend-migration "
        "-p deis-backend-topology "
        "-p deis-control -p deis-store "
        "-p deis-queue -p deis-telemetry -p deis-proxy "
        "--no-fail-fast"
    ),
}
EXPECTED_TEST_PREREQUISITES = {
    "runtime-tests": "cargo build -p deis-proxy",
}
COMPILER_IDENTITY_EVIDENCE_GLOB = "compiler-identity-*.json"
PROVIDER_DRIVER_PORTABILITY_EVIDENCE_GLOB = "provider-driver-portability-*.json"
PROVIDER_DEPLOYMENT_PORTABILITY_EVIDENCE_GLOB = "provider-deployment-portability-*.json"
PROVIDER_DRIVER_RELEASE_PORTABILITY_EVIDENCE_GLOB = (
    "provider-driver-release-portability-*.json"
)
PROVIDER_DRIVER_DISPATCHER_PORTABILITY_EVIDENCE_GLOB = (
    "provider-driver-dispatcher-portability-*.json"
)


class ValidationError(RuntimeError):
    pass


# Legal header status values. A parenthetical suffix (for example an
# "Accepted（partially superseded by …）" note) is tolerated after the value.
ADR_STATUSES = ("Accepted", "Superseded", "Deprecated")
ADR_DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")


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


def adr_header_fields(text: str) -> dict[str, str]:
    """Collect the `- key：value` header lines between the title and first section."""
    fields: dict[str, str] = {}
    for line in text.splitlines()[1:]:
        if line.startswith("## "):
            break
        match = re.fullmatch(r"- ([^：]+)：(.*)", line.strip())
        if match:
            fields.setdefault(match.group(1).strip(), match.group(2).strip())
    return fields


def validate_adrs() -> int:
    adr_dir = ROOT / "docs/adr"
    files = sorted(adr_dir.glob("[0-9][0-9][0-9]-*.md"))
    require(files, "no ADRs found under docs/adr")
    numbers = [int(path.name[:3]) for path in files]
    require(
        numbers == list(range(1, len(numbers) + 1)),
        f"ADR numbers must be contiguous from 001, got {numbers}",
    )

    for path, number in zip(files, numbers, strict=True):
        text = path.read_text(encoding="utf-8")
        relative = path.relative_to(ROOT)
        require(text.startswith(f"# ADR-{number:03d}："), f"{relative}: title number mismatch")
        fields = adr_header_fields(text)
        status = fields.get("状态")
        require(status is not None, f"{relative}: header field 状态 is missing")
        status_value = re.split(r"[（(]", status, maxsplit=1)[0].strip()
        require(
            status_value in ADR_STATUSES,
            f"{relative}: header field 状态 must be one of {ADR_STATUSES}, got {status_value}",
        )
        date = fields.get("日期")
        require(date is not None, f"{relative}: header field 日期 is missing")
        require(
            ADR_DATE.fullmatch(date) is not None,
            f"{relative}: header field 日期 must be an ISO date, got {date}",
        )
        require("关联" in fields, f"{relative}: header field 关联 is missing")
        require("/tmp/" not in text, f"{relative}: temporary path remains")

    index = read_text("docs/adr/README.md")
    for number in numbers:
        require(f"| [{number:03d}](" in index, f"ADR index is missing {number:03d}")
    adr_005 = read_text("docs/adr/005-portable-guarantees.md")
    adr_022 = read_text("docs/adr/022-entry-plan-resource-baseline.md")
    require("ADR-022" in adr_005, "ADR-005 must point at its superseding resource baseline decision")
    require("ADR-005" in adr_022, "ADR-022 must point back at the decision it partially supersedes")
    return len(files)


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
        "environmentRequirementsPath": "standard/requirements/environment-secrets.zh.md",
        "environmentDeclarationsSchemaPath": "schemas/environment-secrets.schema.json",
        "environmentBindingSnapshotSchemaPath": "schemas/environment-binding-snapshot.schema.json",
        "environmentHarnessPath": "conformance/harness/environment-secrets/harness.json",
        "environmentPlatformEvidencePath": "conformance/evidence/environment-secrets-platforms-2026-09-05.json",
        "deploymentRequirementsPath": "standard/requirements/deployment-preview-rollback.zh.md",
        "deploymentPlanSchemaPath": "schemas/deployment-plan.schema.json",
        "deploymentStatusSchemaPath": "schemas/deployment-status.schema.json",
        "deploymentHarnessPath": "conformance/harness/deployment-preview-rollback/harness.json",
        "deploymentPlatformEvidencePath": "conformance/evidence/deployment-preview-rollback-platforms-2026-09-05.json",
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
    for key, label in (
        ("contractSha256", "contract"),
        ("requirementsSha256", "requirements registry"),
        ("canonicalArtifactSchemaSha256", "canonical artifact schema"),
    ):
        require(
            bool(SHA256.fullmatch(source.get(key, ""))),
            f"standard lock {label} digest is not SHA-256",
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
    require(
        bool(SHA256.fullmatch(source.get("environmentDeclarationsSchemaSha256", ""))),
        "standard lock environment declaration schema digest is not SHA-256",
    )
    require(
        bool(SHA256.fullmatch(source.get("environmentBindingSnapshotSchemaSha256", ""))),
        "standard lock environment snapshot schema digest is not SHA-256",
    )
    require(
        bool(SHA256.fullmatch(source.get("deploymentPlanSchemaSha256", ""))),
        "standard lock deployment plan schema digest is not SHA-256",
    )
    require(
        bool(SHA256.fullmatch(source.get("deploymentStatusSchemaSha256", ""))),
        "standard lock deployment status schema digest is not SHA-256",
    )
    verification = lock.get("verification", {})
    require(verification.get("upstreamResult") == "passed", "locked upstream governance check did not pass")
    require(verification.get("expectedCapabilityFamilies") == 15, "unexpected capability family count")
    require(verification.get("expectedDraftClauses") == 223, "unexpected draft clause count")
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


def validate_roadmap(locked_standard_commit: str) -> int:
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
    return len(phases)


def validate_compiler_identity_evidence(evidence_path: str) -> None:
    schema = load_json("docs/product/evidence/compiler-identity.schema.json")
    evidence = load_json(evidence_path)
    try:
        jsonschema.validate(
            instance=evidence,
            schema=schema,
            format_checker=jsonschema.FormatChecker(),
        )
    except jsonschema.ValidationError as error:
        location = "/".join(str(part) for part in error.absolute_path) or "<root>"
        raise ValidationError(
            f"{evidence_path} {location}: {error.message}"
        ) from error

    source = evidence["source"]
    require(
        source["repository"] == "deislet/deislet",
        f"{evidence_path}: unexpected source repository",
    )
    expected_commit = source["commit"]
    expected_archive = source["archive"]["sha256"]
    fixture_root = (ROOT / evidence["fixture"]["path"]).resolve()
    require(
        fixture_root.is_relative_to(ROOT.resolve()) and fixture_root.is_dir(),
        f"{evidence_path}: fixture path escapes or is missing",
    )
    for relative, expected_digest in evidence["fixture"]["files"].items():
        fixture_file = (fixture_root / relative).resolve()
        require(
            fixture_file.is_relative_to(fixture_root) and fixture_file.is_file(),
            f"{evidence_path}: fixture file escapes or is missing: {relative}",
        )
        require(
            hashlib.sha256(fixture_file.read_bytes()).hexdigest() == expected_digest,
            f"{evidence_path}: fixture digest drifted: {relative}",
        )

    observations = evidence["observations"]
    ids = [item["id"] for item in observations]
    aliases = [item["hostAlias"] for item in observations]
    require(len(ids) == len(set(ids)), f"{evidence_path}: duplicate observation id")
    require(
        len(aliases) == len(set(aliases)),
        f"{evidence_path}: duplicate host alias",
    )
    expected_identities = evidence["expectedIdentities"]
    for observation in observations:
        require(
            observation["commit"] == expected_commit,
            f"{evidence_path}: {observation['id']} commit drifted",
        )
        require(
            observation["archiveSha256"] == expected_archive,
            f"{evidence_path}: {observation['id']} archive drifted",
        )
        if observation["result"] == "passed":
            require(
                observation["identities"] == expected_identities,
                f"{evidence_path}: {observation['id']} identity mismatch",
            )

    required_systems = evidence["requiredOperatingSystems"]
    require(
        required_systems == ["linux", "macos", "windows"],
        f"{evidence_path}: required OS set or order drifted",
    )
    passing_systems = {
        item["operatingSystem"] for item in observations if item["result"] == "passed"
    }
    missing = [system for system in required_systems if system not in passing_systems]
    require(
        evidence["assertions"]["missingPassingOperatingSystems"] == missing,
        f"{evidence_path}: missing OS assertion does not match observations",
    )
    expected_outcome = (
        "failed"
        if any(item["result"] == "failed" for item in observations)
        else "passed" if not missing else "partial"
    )
    require(
        evidence["outcome"] == expected_outcome,
        f"{evidence_path}: outcome must be {expected_outcome}",
    )


def validate_provider_driver_portability_evidence(evidence_path: str) -> None:
    schema = load_json("docs/product/evidence/provider-driver-portability.schema.json")
    evidence = load_json(evidence_path)
    validator = jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker())
    errors = sorted(validator.iter_errors(evidence), key=lambda error: list(error.path))
    if errors:
        error = errors[0]
        location = "/" + "/".join(str(part) for part in error.path)
        raise ValidationError(f"{evidence_path} {location}: {error.message}")

    source = evidence["source"]
    observations = evidence["observations"]
    ids = [observation["id"] for observation in observations]
    aliases = [observation["hostAlias"] for observation in observations]
    operating_systems = [observation["operatingSystem"] for observation in observations]
    require(len(ids) == len(set(ids)), f"{evidence_path}: duplicate observation id")
    require(len(aliases) == len(set(aliases)), f"{evidence_path}: duplicate host alias")
    require(
        operating_systems == evidence["requiredOperatingSystems"],
        f"{evidence_path}: operating system observation set or order drifted",
    )
    require(
        all(observation["commit"] == source["commit"] for observation in observations),
        f"{evidence_path}: observation commit drifted",
    )
    passing_systems = {
        observation["operatingSystem"]
        for observation in observations
        if observation["test"]["outcome"] == "passed"
        and observation["lint"]["outcome"] == "passed"
    }
    required_systems = set(evidence["requiredOperatingSystems"])
    all_required_passed = required_systems <= passing_systems
    require(
        evidence["assertions"]["allRequiredOperatingSystemsPassed"] == all_required_passed,
        f"{evidence_path}: required OS assertion does not match observations",
    )
    any_failed = any(
        observation[check]["outcome"] == "failed"
        for observation in observations
        for check in ("test", "lint")
    )
    expected_outcome = "failed" if any_failed else "passed" if all_required_passed else "partial"
    require(
        evidence["outcome"] == expected_outcome,
        f"{evidence_path}: outcome must be {expected_outcome}",
    )


def validate_provider_deployment_portability_evidence(evidence_path: str) -> None:
    schema = load_json("docs/product/evidence/provider-deployment-portability.schema.json")
    evidence = load_json(evidence_path)
    validator = jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker())
    errors = sorted(validator.iter_errors(evidence), key=lambda error: list(error.path))
    if errors:
        error = errors[0]
        location = "/" + "/".join(str(part) for part in error.path)
        raise ValidationError(f"{evidence_path} {location}: {error.message}")

    source = evidence["source"]
    observations = evidence["observations"]
    command_ids = [command["id"] for command in evidence["commands"]]
    require(len(command_ids) == len(set(command_ids)), f"{evidence_path}: duplicate command id")

    ids = [observation["id"] for observation in observations]
    aliases = [observation["hostAlias"] for observation in observations]
    require(len(ids) == len(set(ids)), f"{evidence_path}: duplicate observation id")
    require(len(aliases) == len(set(aliases)), f"{evidence_path}: duplicate host alias")
    require(
        all(observation["commit"] == source["commit"] for observation in observations),
        f"{evidence_path}: observation commit drifted",
    )

    passing_systems: set[str] = set()
    for observation in observations:
        checks = observation["checks"]
        check_ids = [check["id"] for check in checks]
        require(
            check_ids == command_ids,
            f"{evidence_path}: {observation['id']} check set or order drifted",
        )
        for check in checks:
            if check["id"] != "strict-clippy":
                require(
                    all(field in check for field in ("passed", "failed", "ignored")),
                    f"{evidence_path}: {observation['id']} {check['id']} lacks test counts",
                )
            if check["outcome"] == "passed" and "failed" in check:
                require(
                    check["failed"] == 0,
                    f"{evidence_path}: {observation['id']} {check['id']} passed with failures",
                )
        if all(check["outcome"] == "passed" for check in checks):
            passing_systems.add(observation["operatingSystem"])

    required_systems = set(evidence["requiredOperatingSystems"])
    all_required_passed = required_systems <= passing_systems
    require(
        evidence["assertions"]["allRequiredOperatingSystemsPassed"] == all_required_passed,
        f"{evidence_path}: required OS assertion does not match observations",
    )
    windows_passed = "windows" in passing_systems
    require(
        evidence["assertions"]["windowsSignalPortabilityRegressionClosed"] == windows_passed,
        f"{evidence_path}: Windows signal regression assertion does not match observations",
    )
    any_failed = any(
        check["outcome"] == "failed"
        for observation in observations
        for check in observation["checks"]
    )
    expected_outcome = "failed" if any_failed else "passed" if all_required_passed else "partial"
    require(
        evidence["outcome"] == expected_outcome,
        f"{evidence_path}: outcome must be {expected_outcome}",
    )


def validate_provider_driver_release_portability_evidence(evidence_path: str) -> None:
    schema = load_json(
        "docs/product/evidence/provider-driver-release-portability.schema.json"
    )
    evidence = load_json(evidence_path)
    validator = jsonschema.Draft202012Validator(
        schema, format_checker=jsonschema.FormatChecker()
    )
    errors = sorted(validator.iter_errors(evidence), key=lambda error: list(error.path))
    if errors:
        error = errors[0]
        location = "/" + "/".join(str(part) for part in error.path)
        raise ValidationError(f"{evidence_path} {location}: {error.message}")

    source = evidence["source"]
    observations = evidence["observations"]
    ids = [observation["id"] for observation in observations]
    aliases = [observation["hostAlias"] for observation in observations]
    operating_systems = [observation["operatingSystem"] for observation in observations]
    require(len(ids) == len(set(ids)), f"{evidence_path}: duplicate observation id")
    require(len(aliases) == len(set(aliases)), f"{evidence_path}: duplicate host alias")
    require(
        operating_systems == evidence["requiredOperatingSystems"],
        f"{evidence_path}: operating system observation set or order drifted",
    )
    require(
        all(observation["commit"] == source["commit"] for observation in observations),
        f"{evidence_path}: observation commit drifted",
    )
    exact_archive_verified = all(
        observation["archiveSha256"] == source["archive"]["sha256"]
        for observation in observations
    )
    require(
        evidence["assertions"]["exactArchiveVerifiedOnEveryHost"]
        == exact_archive_verified,
        f"{evidence_path}: exact archive assertion does not match observations",
    )
    passing_systems = {
        observation["operatingSystem"]
        for observation in observations
        if observation["test"]["outcome"] == "passed"
        and observation["lint"]["outcome"] == "passed"
    }
    required_systems = set(evidence["requiredOperatingSystems"])
    all_required_passed = required_systems <= passing_systems
    require(
        evidence["assertions"]["allRequiredOperatingSystemsPassed"]
        == all_required_passed,
        f"{evidence_path}: required OS assertion does not match observations",
    )
    for observation in observations:
        test = observation["test"]
        require(
            all(field in test for field in ("passed", "failed", "ignored")),
            f"{evidence_path}: {observation['id']} lacks test counts",
        )
        if test["outcome"] == "passed":
            require(
                test["failed"] == 0,
                f"{evidence_path}: {observation['id']} passed with failures",
            )
    prior_attempts = evidence["priorFailedAttempts"]
    prior_commits = [prior["commit"] for prior in prior_attempts]
    require(
        len(prior_commits) == len(set(prior_commits)),
        f"{evidence_path}: duplicate prior failure commit",
    )
    for prior in prior_attempts:
        require(
            prior["fixedBy"] == source["commit"],
            f"{evidence_path}: prior failure is not linked to the validated fix",
        )
        require(
            prior["commit"] != source["commit"],
            f"{evidence_path}: prior failure cannot be the passing source commit",
        )
        checks = prior["checks"]
        require(
            [check["id"] for check in checks] == ["test", "clippy"],
            f"{evidence_path}: prior check set or order drifted",
        )
        require(
            any(check["outcome"] == "failed" for check in checks),
            f"{evidence_path}: prior attempt has no failed check",
        )
    any_failed = any(
        observation[check]["outcome"] == "failed"
        for observation in observations
        for check in ("test", "lint")
    )
    expected_outcome = "failed" if any_failed else "passed" if all_required_passed else "partial"
    require(
        evidence["outcome"] == expected_outcome,
        f"{evidence_path}: outcome must be {expected_outcome}",
    )


def validate_provider_driver_dispatcher_portability_evidence(
    evidence_path: str,
) -> None:
    schema = load_json(
        "docs/product/evidence/provider-driver-dispatcher-portability.schema.json"
    )
    evidence = load_json(evidence_path)
    validator = jsonschema.Draft202012Validator(
        schema, format_checker=jsonschema.FormatChecker()
    )
    errors = sorted(validator.iter_errors(evidence), key=lambda error: list(error.path))
    if errors:
        error = errors[0]
        location = "/" + "/".join(str(part) for part in error.path)
        raise ValidationError(f"{evidence_path} {location}: {error.message}")

    source = evidence["source"]
    observations = evidence["observations"]
    command_ids = [command["id"] for command in evidence["commands"]]
    require(
        len(command_ids) == len(set(command_ids)),
        f"{evidence_path}: duplicate command id",
    )
    ids = [observation["id"] for observation in observations]
    aliases = [observation["hostAlias"] for observation in observations]
    operating_systems = [
        observation["operatingSystem"] for observation in observations
    ]
    require(len(ids) == len(set(ids)), f"{evidence_path}: duplicate observation id")
    require(
        len(aliases) == len(set(aliases)),
        f"{evidence_path}: duplicate host alias",
    )
    require(
        operating_systems == evidence["requiredOperatingSystems"],
        f"{evidence_path}: operating system observation set or order drifted",
    )

    passing_systems: set[str] = set()
    exact_archive_verified = True
    for observation in observations:
        require(
            observation["commit"] == source["commit"],
            f"{evidence_path}: {observation['id']} commit drifted",
        )
        exact_archive_verified &= (
            observation["archiveSha256"] == source["archive"]["sha256"]
        )
        checks = observation["checks"]
        require(
            [check["id"] for check in checks] == command_ids,
            f"{evidence_path}: {observation['id']} check set or order drifted",
        )
        for check in checks:
            if check["id"] != "strict-clippy":
                require(
                    all(field in check for field in ("passed", "failed", "ignored")),
                    f"{evidence_path}: {observation['id']} {check['id']} lacks test counts",
                )
            if check["outcome"] == "passed" and "failed" in check:
                require(
                    check["failed"] == 0,
                    f"{evidence_path}: {observation['id']} {check['id']} passed with failures",
                )
        if all(check["outcome"] == "passed" for check in checks):
            passing_systems.add(observation["operatingSystem"])

    require(
        evidence["assertions"]["exactArchiveVerifiedOnEveryHost"]
        == exact_archive_verified,
        f"{evidence_path}: exact archive assertion does not match observations",
    )
    required_systems = set(evidence["requiredOperatingSystems"])
    all_required_passed = required_systems <= passing_systems
    require(
        evidence["assertions"]["allRequiredOperatingSystemsPassed"]
        == all_required_passed,
        f"{evidence_path}: required OS assertion does not match observations",
    )
    require(
        evidence["assertions"]["windowsAsyncSendRegressionClosed"]
        == ("windows" in passing_systems),
        f"{evidence_path}: Windows async Send assertion does not match observations",
    )

    prior_commits: set[str] = set()
    for prior in evidence["priorFailedAttempts"]:
        prior_source = prior["source"]
        require(
            prior_source["commit"] not in prior_commits,
            f"{evidence_path}: duplicate prior failure commit",
        )
        prior_commits.add(prior_source["commit"])
        require(
            prior_source["commit"] != source["commit"]
            and prior["fixedBy"] == source["commit"],
            f"{evidence_path}: prior failure is not linked to the passing source",
        )
        require(
            [check["id"] for check in prior["checks"]] == command_ids,
            f"{evidence_path}: prior failure check set or order drifted",
        )
        require(
            any(check["outcome"] == "failed" for check in prior["checks"]),
            f"{evidence_path}: prior attempt has no failed check",
        )

    any_failed = any(
        check["outcome"] == "failed"
        for observation in observations
        for check in observation["checks"]
    )
    expected_outcome = (
        "failed" if any_failed else "passed" if all_required_passed else "partial"
    )
    require(
        evidence["outcome"] == expected_outcome,
        f"{evidence_path}: outcome must be {expected_outcome}",
    )


def validate_integration() -> None:
    control_manifest = tomllib.loads(read_text("crates/deis-control/Cargo.toml"))
    control_dependencies = set(control_manifest.get("dependencies", {}))
    require(
        "deis-artifact" in control_dependencies,
        "Control must use the shared artifact integrity boundary",
    )
    require(
        not control_dependencies.intersection(
            {
                "deis-build",
                "deis-build-core",
                "deis-build-adapter-cloudflare",
                "deis-build-adapter-deislet",
                "deis-build-adapter-deno",
                "deis-build-adapter-tencent",
            }
        ),
        "Control must not link compiler or provider-adapter crates",
    )
    artifact_manifest = tomllib.loads(read_text("crates/deis-artifact/Cargo.toml"))
    artifact_dependencies = set(artifact_manifest.get("dependencies", {}))
    require(
        not any(
            dependency.startswith("deis-build")
            for dependency in artifact_dependencies
        ),
        "the artifact integrity crate must not depend on the compiler layer",
    )
    build_manifest = tomllib.loads(read_text("crates/deis-build/Cargo.toml"))
    require(
        "deis-artifact" in build_manifest.get("dependencies", {}),
        "the compiler must reuse the shared artifact integrity boundary",
    )

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
        "python3 scripts/check-provider-tool-lock.py" in workflow,
        "CI does not validate official provider tool source pins",
    )
    require(
        "python3 scripts/generate-target-status.py --check" in workflow,
        "CI does not reject registry-derived target status drift",
    )
    require((ROOT / "contracts/v1/registry.json").is_file(), "shared product contract registry is missing")
    require((ROOT / "contracts/v1/README.zh.md").is_file(), "shared product contract documentation is missing")
    provider_driver_manifest = tomllib.loads(
        read_text("crates/deis-provider-driver/Cargo.toml")
    )
    provider_driver_dependencies = set(provider_driver_manifest.get("dependencies", {}))
    require(
        not provider_driver_dependencies.intersection(
            {
                "deis-build",
                "deis-build-core",
                "deis-build-adapter-cloudflare",
                "deis-build-adapter-deislet",
                "deis-build-adapter-deno",
                "deis-build-adapter-tencent",
                "deis-control",
            }
        ),
        "provider-neutral driver contract must not depend on compiler, adapters, or Control",
    )
    require(
        (ROOT / "provider-drivers/official-tool-lock.json").is_file()
        and (ROOT / "provider-drivers/official-tool-lock.schema.json").is_file(),
        "official provider tool source lock or schema is missing",
    )
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
    for vendor in ("cloudflare", "deno", "tencent"):
        artifact_test = ROOT / f"crates/deis-build-adapter-{vendor}/tests/artifact_shape.rs"
        require(
            artifact_test.is_file(),
            f"{artifact_test.relative_to(ROOT)} is missing; adapter artifact checks must not be named conformance",
        )


def main() -> int:
    try:
        adr_count = validate_adrs()
        commit = validate_standard_lock()
        phase_count = validate_roadmap(commit)
        evidence_paths = sorted(
            path.relative_to(ROOT).as_posix()
            for path in (ROOT / "docs/product/evidence").glob(
                COMPILER_IDENTITY_EVIDENCE_GLOB
            )
            if path.name != "compiler-identity.schema.json"
        )
        require(evidence_paths, "no real compiler identity evidence is registered")
        for evidence_path in evidence_paths:
            validate_compiler_identity_evidence(evidence_path)
        portability_paths = sorted(
            path.relative_to(ROOT).as_posix()
            for path in (ROOT / "docs/product/evidence").glob(
                PROVIDER_DRIVER_PORTABILITY_EVIDENCE_GLOB
            )
            if path.name != "provider-driver-portability.schema.json"
        )
        require(portability_paths, "no provider driver portability evidence is registered")
        for evidence_path in portability_paths:
            validate_provider_driver_portability_evidence(evidence_path)
        deployment_portability_paths = sorted(
            path.relative_to(ROOT).as_posix()
            for path in (ROOT / "docs/product/evidence").glob(
                PROVIDER_DEPLOYMENT_PORTABILITY_EVIDENCE_GLOB
            )
            if path.name != "provider-deployment-portability.schema.json"
        )
        require(
            deployment_portability_paths,
            "no provider deployment portability evidence is registered",
        )
        for evidence_path in deployment_portability_paths:
            validate_provider_deployment_portability_evidence(evidence_path)
        release_portability_paths = sorted(
            path.relative_to(ROOT).as_posix()
            for path in (ROOT / "docs/product/evidence").glob(
                PROVIDER_DRIVER_RELEASE_PORTABILITY_EVIDENCE_GLOB
            )
            if path.name != "provider-driver-release-portability.schema.json"
        )
        require(
            release_portability_paths,
            "no provider driver release portability evidence is registered",
        )
        for evidence_path in release_portability_paths:
            validate_provider_driver_release_portability_evidence(evidence_path)
        dispatcher_portability_paths = sorted(
            path.relative_to(ROOT).as_posix()
            for path in (ROOT / "docs/product/evidence").glob(
                PROVIDER_DRIVER_DISPATCHER_PORTABILITY_EVIDENCE_GLOB
            )
            if path.name != "provider-driver-dispatcher-portability.schema.json"
        )
        require(
            dispatcher_portability_paths,
            "no provider driver dispatcher portability evidence is registered",
        )
        for evidence_path in dispatcher_portability_paths:
            validate_provider_driver_dispatcher_portability_evidence(evidence_path)
        validate_integration()
    except (KeyError, TypeError, ValidationError) as error:
        print(f"product governance validation failed: {error}", file=sys.stderr)
        return 1

    print(
        f"product governance validation passed: {adr_count} ADRs, {phase_count} roadmap phases, "
        f"Edge Canon {commit[:7]} locked, no temporary paths"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
