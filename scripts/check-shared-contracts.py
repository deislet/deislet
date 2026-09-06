#!/usr/bin/env python3
"""Execute the draft v1 shared-model schemas and cross-document invariants."""

from __future__ import annotations

import copy
import base64
import hashlib
import json
import sys
from collections import defaultdict, deque
from pathlib import Path

try:
    import jsonschema
except ImportError:
    print(
        "shared contract validation requires jsonschema; "
        "install scripts/governance-requirements.txt",
        file=sys.stderr,
    )
    raise SystemExit(2)


ROOT = Path(__file__).resolve().parents[1]
REGISTRY_PATH = "contracts/v1/registry.json"
EXPECTED_DOCUMENTS = {
    "resource-identity",
    "canonical-artifact",
    "binding-snapshot",
    "operation",
    "provider-driver",
    "provider-driver-release",
    "provider-driver-trust-root",
    "provider-driver-trust-root-envelope",
    "provider-driver-release-authorization",
    "provider-driver-release-envelope",
    "provider-driver-transparency-proof",
    "provider-driver-process",
    "identity-migration",
    "backend-migration-evidence",
    "backend-restore-set",
    "backend-restore-report",
    "control-list-page",
    "control-error",
}
EXPECTED_PROVIDER_OPERATIONS = [
    "preflight",
    "plan",
    "prepare",
    "upload",
    "verify",
    "activate",
    "observe",
    "rollback",
    "inspect",
    "reconcile",
    "delete",
    "cleanup",
]
EXPECTED_MIGRATION_PHASES = [
    "inventory",
    "allocate-canonical-ids",
    "dual-read",
    "copy-or-rename",
    "verify",
    "cutover",
    "revoke-legacy-capabilities",
    "tombstone",
]
CONTROL_ERROR_SCHEMA = "contracts/v1/control-error.schema.json"
# Failure classes whose contract is "the identical request may succeed later".
# Every other enumerated code is a caller-visible defect and must never invite
# a blind retry. Kept in step with `codes::is_retryable` in
# crates/deis-control/src/api/error.rs and the `ErrorCode` description in
# contracts/openapi/v1.json.
RETRYABLE_CONTROL_ERROR_CODES = {
    "service-unavailable",
    "backend-unavailable",
    "database-unavailable",
    "idempotency-request-in-progress",
}
UUID7_PATTERN = (
    "^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-"
    "[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)


class ContractError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ContractError(message)


def load_json(relative: str) -> dict:
    path = ROOT / relative
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"{relative}: {error}") from error
    require(isinstance(value, dict), f"{relative}: top-level JSON must be an object")
    return value


def apply_mutation(document: object, mutation: dict) -> None:
    pointer = mutation.get("pointer")
    require(isinstance(pointer, str) and pointer.startswith("/"), "fixture mutation needs a JSON pointer")
    parts = [part.replace("~1", "/").replace("~0", "~") for part in pointer[1:].split("/")]
    parent = document
    for part in parts[:-1]:
        parent = parent[int(part)] if isinstance(parent, list) else parent[part]
    leaf = parts[-1]
    operation = mutation.get("op")
    if operation == "remove":
        if isinstance(parent, list):
            parent.pop(int(leaf))
        else:
            del parent[leaf]
    elif operation in {"add", "replace"}:
        if isinstance(parent, list):
            index = int(leaf)
            if operation == "add":
                parent.insert(index, mutation.get("value"))
            else:
                parent[index] = mutation.get("value")
        else:
            if operation == "replace":
                require(leaf in parent, f"fixture replace target does not exist: {pointer}")
            parent[leaf] = mutation.get("value")
    else:
        raise ContractError(f"unsupported fixture mutation operation: {operation}")


def fixture_document(case: dict) -> dict:
    document_path = case.get("document")
    derived_path = case.get("derivedFrom")
    require(bool(document_path) != bool(derived_path), f"{case.get('id')}: choose document or derivedFrom")
    document = copy.deepcopy(load_json(document_path or derived_path))
    mutations = case.get("mutations", [])
    if document_path:
        require(not mutations, f"{case.get('id')}: direct fixture must not have mutations")
    else:
        require(mutations, f"{case.get('id')}: derived fixture needs a mutation")
    for mutation in mutations:
        apply_mutation(document, mutation)
    return document


def validate_schemas_and_fixtures(registry: dict) -> dict[str, dict]:
    registry_schema = load_json("contracts/v1/registry.schema.json")
    jsonschema.Draft202012Validator.check_schema(registry_schema)
    jsonschema.Draft202012Validator(
        registry_schema, format_checker=jsonschema.FormatChecker()
    ).validate(registry)

    documents = registry.get("documents", [])
    document_ids = [document.get("id") for document in documents]
    require(set(document_ids) == EXPECTED_DOCUMENTS, "shared contract document set differs from v1")
    require(len(document_ids) == len(set(document_ids)), "shared contract document IDs are duplicated")

    schemas: dict[str, dict] = {}
    known_paths: set[str] = set()
    for document in documents:
        schema_path = document["schema"]
        require(schema_path not in known_paths, f"schema appears twice in registry: {schema_path}")
        schema = load_json(schema_path)
        jsonschema.Draft202012Validator.check_schema(schema)
        schemas[schema_path] = schema
        known_paths.add(schema_path)
        canonical = schema.get("$defs", {}).get("canonicalId")
        if canonical is not None:
            require(canonical.get("pattern") == UUID7_PATTERN, f"{schema_path}: canonical ID rule drifted")

    fixture_index = load_json(registry["fixtures"])
    require(fixture_index.get("schemaVersion") == 1, "fixture index schemaVersion must be 1")
    cases = fixture_index.get("cases", [])
    case_ids = [case.get("id") for case in cases]
    require(cases and len(case_ids) == len(set(case_ids)), "fixture IDs must be present and unique")
    outcomes: defaultdict[str, set[bool]] = defaultdict(set)
    for case in cases:
        case_id = case["id"]
        schema_path = case.get("schema")
        require(schema_path in schemas, f"{case_id}: fixture uses an unregistered schema")
        expected = case.get("expectedValid")
        require(isinstance(expected, bool), f"{case_id}: expectedValid must be boolean")
        document = fixture_document(case)
        errors = list(
            jsonschema.Draft202012Validator(
                schemas[schema_path], format_checker=jsonschema.FormatChecker()
            ).iter_errors(document)
        )
        actual = not errors
        if actual != expected:
            detail = "valid" if actual else "; ".join(error.message for error in errors[:3])
            raise ContractError(f"{case_id}: expected valid={expected}, got {detail}")
        outcomes[schema_path].add(expected)

    for schema_path in schemas:
        require(outcomes[schema_path] == {False, True}, f"{schema_path}: needs positive and negative fixtures")
    return schemas


def validate_operation_transitions(registry: dict, operation_schema: dict) -> None:
    table = load_json(registry["operationTransitions"])
    states = set(operation_schema["properties"]["state"]["enum"])
    initial = table.get("initialState")
    terminal = set(table.get("terminalStates", []))
    require(initial in states, "operation initial state is unknown")
    require(terminal == {"succeeded", "failed", "cancelled"}, "operation terminal state set drifted")

    transitions = table.get("transitions", [])
    pairs: set[tuple[str, str]] = set()
    outgoing: defaultdict[str, set[str]] = defaultdict(set)
    adjacency: defaultdict[str, set[str]] = defaultdict(set)
    for transition in transitions:
        source = transition.get("from")
        event = transition.get("event")
        target = transition.get("to")
        require(source in states and target in states, f"unknown operation transition: {transition}")
        require((source, event) not in pairs, f"non-deterministic operation transition: {source}/{event}")
        require(source not in terminal, f"terminal operation state has an outgoing transition: {source}")
        pairs.add((source, event))
        outgoing[source].add(target)
        adjacency[source].add(target)

    reached = {initial}
    queue = deque([initial])
    while queue:
        source = queue.popleft()
        for target in adjacency[source]:
            if target not in reached:
                reached.add(target)
                queue.append(target)
    require(reached == states, f"unreachable operation states: {sorted(states - reached)}")

    for state in states - terminal:
        seen = {state}
        queue = deque([state])
        can_finish = False
        while queue:
            source = queue.popleft()
            for target in adjacency[source]:
                if target in terminal:
                    can_finish = True
                elif target not in seen:
                    seen.add(target)
                    queue.append(target)
        require(can_finish, f"operation state cannot reach a terminal state: {state}")


def validate_control_error_invariants(registry: dict) -> None:
    """The retryable flag is a property of the error code, not of a response."""
    schema = load_json(CONTROL_ERROR_SCHEMA)
    codes = set(schema["properties"]["error"]["properties"]["code"]["enum"])
    require(
        RETRYABLE_CONTROL_ERROR_CODES <= codes,
        "Control error envelope dropped a retryable failure class",
    )
    covered: set[str] = set()
    for case in load_json(registry["fixtures"])["cases"]:
        if case.get("schema") != CONTROL_ERROR_SCHEMA or not case.get("expectedValid"):
            continue
        error = fixture_document(case)["error"]
        code = error["code"]
        expected = code in RETRYABLE_CONTROL_ERROR_CODES
        require(
            error["retryable"] is expected,
            f"{case['id']}: {code} must report retryable={expected}",
        )
        require(
            expected or "retryAfterSeconds" not in error,
            f"{case['id']}: {code} is not retryable and must not advise a retry delay",
        )
        covered.add(code)
    require(
        bool(covered & RETRYABLE_CONTROL_ERROR_CODES) and bool(covered - RETRYABLE_CONTROL_ERROR_CODES),
        "Control error fixtures must cover a retryable and a non-retryable failure class",
    )


def validate_cross_document_invariants(registry: dict) -> None:
    require(registry.get("identifierScheme") == "uuidv7-lowercase", "v1 identity scheme drifted")
    require(registry.get("requiredProviderOperations") == EXPECTED_PROVIDER_OPERATIONS, "provider operation order/set drifted")
    require(registry.get("identityMigrationPhases") == EXPECTED_MIGRATION_PHASES, "identity migration phases drifted")

    lock = load_json(registry["standardLock"])
    artifact = load_json("contracts/v1/fixtures/valid/canonical-artifact.json")
    require(
        artifact["standard"]["sourceCommit"] == lock["source"]["sourceCommit"],
        "artifact fixture does not pin the locked Edge Canon commit",
    )
    require(
        artifact["standard"]["version"]
        == f"edge-canon.next@{lock['source']['sourceCommit']}",
        "artifact fixture standard version is not the exact locked Edge Canon version",
    )
    require(
        artifact["standard"]["artifactSchemaPath"]
        == lock["source"]["canonicalArtifactSchemaPath"],
        "artifact fixture points at a different upstream canonical output schema",
    )
    require(
        artifact["standard"]["artifactSchemaDigest"]["value"]
        == lock["source"]["canonicalArtifactSchemaSha256"],
        "artifact fixture canonical output schema digest differs from the standard lock",
    )
    require(
        artifact["canonicalOutput"]["storage"]["locator"].endswith(
            artifact["canonicalOutput"]["artifactSha256"]
        ),
        "artifact fixture storage locator is not content-addressed by its manifest identity",
    )
    require(artifact.get("deployable") is False, "proposal artifact fixture must remain non-deployable")

    binding = load_json("contracts/v1/fixtures/valid/binding-snapshot.json")
    require(
        binding["standardVersion"] == artifact["standard"]["version"],
        "binding snapshot and canonical artifact pin different standard versions",
    )
    require(
        all(
            entry["resolvedGuarantee"]["standardVersion"] == binding["standardVersion"]
            for entry in binding["resolutions"]
        ),
        "binding resolution guarantee differs from its snapshot standard version",
    )
    declarations = {entry["bindingId"]: (entry["name"], entry["kind"]) for entry in binding["declarations"]}
    resolutions = {entry["bindingId"]: (entry["name"], entry["kind"]) for entry in binding["resolutions"]}
    require(declarations == resolutions, "binding declarations and resolutions differ")

    driver = load_json("contracts/v1/fixtures/valid/provider-driver.json")
    require(driver.get("operations") == EXPECTED_PROVIDER_OPERATIONS, "provider fixture omits a required operation")
    require(driver.get("supportedStandardVersions") == [], "proposal driver must not claim a supported standard version")

    driver_release = load_json("contracts/v1/fixtures/valid/provider-driver-release.json")
    release_canonical = json.dumps(
        driver_release, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    require(
        driver["driver"]["distributionDigest"]["value"]
        == hashlib.sha256(release_canonical).hexdigest(),
        "provider driver distribution digest must bind the canonical release index",
    )
    require(
        driver_release["driver"]
        == {"id": driver["driver"]["id"], "version": driver["driver"]["version"]},
        "provider driver release identity differs from its manifest",
    )
    require(
        driver_release["driverContractVersion"] == driver["driverContractVersion"],
        "provider driver release and manifest use different contract versions",
    )
    distributions = driver_release["distributions"]
    hosts = [distribution["hostOperatingSystem"] for distribution in distributions]
    require(
        hosts == driver["supportedHostOperatingSystems"] and len(hosts) == len(set(hosts)),
        "provider driver release must cover each declared host exactly once and in canonical order",
    )
    expected_tools = sorted(
        (
            tool["kind"],
            tool["name"],
            tool["version"],
            tool["distributionDigest"]["value"],
        )
        for tool in driver["officialTools"]
    )
    for distribution in distributions:
        files = distribution["files"]
        paths = [entry["path"] for entry in files]
        require(
            paths == sorted(paths) and len(paths) == len(set(paths)),
            f"provider driver release {distribution['hostOperatingSystem']} file table is not canonical",
        )
        file_table_canonical = json.dumps(
            files, ensure_ascii=False, separators=(",", ":"), sort_keys=True
        ).encode("utf-8")
        require(
            distribution["treeDigest"]["value"]
            == hashlib.sha256(file_table_canonical).hexdigest(),
            f"provider driver release {distribution['hostOperatingSystem']} tree digest drifted",
        )
        file_digests = {entry["path"]: entry["sha256"]["value"] for entry in files}
        components = distribution["components"]
        component_keys = [(component["role"], component["name"]) for component in components]
        require(
            len(component_keys) == len(set(component_keys)),
            f"provider driver release {distribution['hostOperatingSystem']} repeats a component",
        )
        driver_components = [component for component in components if component["role"] == "driver"]
        require(
            len(driver_components) == 1
            and driver_components[0]["name"] == driver["driver"]["id"]
            and driver_components[0]["version"] == driver["driver"]["version"],
            f"provider driver release {distribution['hostOperatingSystem']} lacks its exact driver",
        )
        actual_tools = sorted(
            (
                component["toolKind"],
                component["name"],
                component["version"],
                component["sourceDistributionDigest"]["value"],
            )
            for component in components
            if component["role"] == "official-tool"
        )
        require(
            actual_tools == expected_tools,
            f"provider driver release {distribution['hostOperatingSystem']} official tools differ",
        )
        for component in components:
            executable = component.get("executable")
            if executable is not None:
                component_paths = component["filePaths"]
                require(
                    component_paths == sorted(component_paths)
                    and len(component_paths) == len(set(component_paths))
                    and executable["path"] in component_paths,
                    f"provider driver release {distribution['hostOperatingSystem']} component file set is not canonical",
                )
                component_files = [entry for entry in files if entry["path"] in component_paths]
                require(
                    len(component_files) == len(component_paths),
                    f"provider driver release {distribution['hostOperatingSystem']} component references a missing file",
                )
                component_canonical = json.dumps(
                    component_files, ensure_ascii=False, separators=(",", ":"), sort_keys=True
                ).encode("utf-8")
                require(
                    component["materializationDigest"]["value"]
                    == hashlib.sha256(component_canonical).hexdigest(),
                    f"provider driver release {distribution['hostOperatingSystem']} component digest drifted",
                )
                require(
                    file_digests.get(executable["path"]) == executable["sha256"]["value"],
                    f"provider driver release {distribution['hostOperatingSystem']} executable is not pinned in its file table",
                )

    trust_root = load_json("contracts/v1/fixtures/valid/provider-driver-trust-root.json")
    root_role = trust_root["root"]
    signing_role = trust_root["providerDriverRelease"]
    key_ids = signing_role["keyIds"]
    keys = trust_root["keys"]
    require(
        root_role["keyIds"] == sorted(root_role["keyIds"])
        and len(root_role["keyIds"]) == len(set(root_role["keyIds"]))
        and root_role["threshold"] <= len(root_role["keyIds"]),
        "provider driver root signing role is not canonical",
    )
    require(
        key_ids == sorted(key_ids) and len(key_ids) == len(set(key_ids)),
        "provider driver release signing role key IDs are not canonical",
    )
    require(
        [key["keyId"] for key in keys] == sorted(key["keyId"] for key in keys)
        and len({key["keyId"] for key in keys}) == len(keys),
        "provider driver release trust keys are not canonical",
    )
    require(
        signing_role["threshold"] <= len(key_ids)
        and set(key_ids) <= {key["keyId"] for key in keys},
        "provider driver release signing role threshold or keys are invalid",
    )
    require(
        set(root_role["keyIds"]) <= {key["keyId"] for key in keys},
        "provider driver root signing role references an unknown key",
    )
    for key in keys:
        public_key = base64.b64decode(key["publicKey"], validate=True)
        require(
            len(public_key) == 32
            and key["keyId"] == hashlib.sha256(public_key).hexdigest(),
            "provider driver release trust key ID differs from its public key",
        )

    authorization = load_json(
        "contracts/v1/fixtures/valid/provider-driver-release-authorization.json"
    )
    manifest_canonical = json.dumps(
        driver, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    require(
        authorization["subject"]
        == {
            "releaseSha256": {
                "algorithm": "sha256",
                "value": hashlib.sha256(release_canonical).hexdigest(),
            },
            "manifestSha256": {
                "algorithm": "sha256",
                "value": hashlib.sha256(manifest_canonical).hexdigest(),
            },
            "backendId": driver["backend"]["id"],
            "driverId": driver["driver"]["id"],
            "driverVersion": driver["driver"]["version"],
        },
        "provider driver release authorization subject differs from its exact documents",
    )
    authorization_canonical = json.dumps(
        authorization, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    envelope = load_json("contracts/v1/fixtures/valid/provider-driver-release-envelope.json")
    require(
        base64.b64decode(envelope["payload"], validate=True) == authorization_canonical,
        "provider driver release DSSE payload is not the canonical authorization",
    )
    envelope_signers = [signature["keyid"] for signature in envelope["signatures"]]
    require(
        envelope_signers == sorted(envelope_signers)
        and len(envelope_signers) == len(set(envelope_signers))
        and set(envelope_signers) <= set(key_ids)
        and len(envelope_signers) >= signing_role["threshold"],
        "provider driver release DSSE signers do not satisfy the trusted role",
    )

    next_root = load_json("contracts/v1/fixtures/valid/provider-driver-trust-root-v2.json")
    next_root_canonical = json.dumps(
        next_root, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    root_envelope = load_json(
        "contracts/v1/fixtures/valid/provider-driver-trust-root-envelope.json"
    )
    require(
        next_root["rootVersion"] == trust_root["rootVersion"] + 1
        and base64.b64decode(root_envelope["payload"], validate=True)
        == next_root_canonical,
        "provider driver trust root envelope does not carry exactly the next canonical root",
    )
    root_envelope_signers = [
        signature["keyid"] for signature in root_envelope["signatures"]
    ]
    next_root_role = next_root["root"]
    require(
        root_envelope_signers == sorted(root_envelope_signers)
        and len(root_envelope_signers) == len(set(root_envelope_signers))
        and len(set(root_envelope_signers) & set(root_role["keyIds"]))
        >= root_role["threshold"]
        and len(set(root_envelope_signers) & set(next_root_role["keyIds"]))
        >= next_root_role["threshold"],
        "provider driver trust root envelope lacks old-root or new-root threshold",
    )

    transparency_policy = trust_root["providerDriverTransparency"]
    transparency_key_ids = [
        identity["keyId"]
        for identity in transparency_policy["logs"]
        + transparency_policy["witnesses"]
    ]
    require(
        len(transparency_key_ids) == len(set(transparency_key_ids))
        and set(transparency_key_ids) <= {key["keyId"] for key in keys},
        "provider driver transparency identities must use distinct trusted keys",
    )
    require(
        set(transparency_key_ids).isdisjoint(root_role["keyIds"])
        and set(transparency_key_ids).isdisjoint(
            trust_root["providerDriverRelease"]["keyIds"]
        ),
        "provider driver transparency keys overlap root or release signing roles",
    )
    require(
        [
            (entry["operator"], entry["origin"], entry["keyId"])
            for entry in transparency_policy["logs"]
        ]
        == sorted(
            (entry["operator"], entry["origin"], entry["keyId"])
            for entry in transparency_policy["logs"]
        )
        and [
            (entry["operator"], entry["name"], entry["keyId"])
            for entry in transparency_policy["witnesses"]
        ]
        == sorted(
            (entry["operator"], entry["name"], entry["keyId"])
            for entry in transparency_policy["witnesses"]
        ),
        "provider driver transparency identities are not canonically ordered",
    )
    require(
        transparency_policy["logOperatorThreshold"]
        <= len({entry["operator"] for entry in transparency_policy["logs"]})
        and transparency_policy["witnessOperatorThreshold"]
        <= len({entry["operator"] for entry in transparency_policy["witnesses"]}),
        "provider driver transparency thresholds exceed distinct operators",
    )
    require(
        {entry["operator"] for entry in transparency_policy["logs"]}.isdisjoint(
            entry["operator"] for entry in transparency_policy["witnesses"]
        ),
        "provider driver transparency log and witness operators are not independent",
    )
    transparency = load_json(
        "contracts/v1/fixtures/valid/provider-driver-transparency-proof.json"
    )
    envelope_canonical = json.dumps(
        envelope, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    require(
        transparency["authorizationSha256"]
        == {
            "algorithm": "sha256",
            "value": hashlib.sha256(envelope_canonical).hexdigest(),
        },
        "provider driver transparency proof does not bind the canonical authorization envelope",
    )
    proof_origins = [entry["logOrigin"] for entry in transparency["entries"]]
    require(
        proof_origins == sorted(set(proof_origins)),
        "provider driver transparency proof origins are not unique and ordered",
    )
    leaf_hash = hashlib.sha256(b"\x00" + envelope_canonical).digest()
    fixture_entry = transparency["entries"][0]
    checkpoint_lines = fixture_entry["checkpoint"].splitlines()
    require(
        fixture_entry["logIndex"] == 0
        and fixture_entry["inclusionPath"] == []
        and checkpoint_lines[:3]
        == [
            fixture_entry["logOrigin"],
            "1",
            base64.b64encode(leaf_hash).decode("ascii"),
        ],
        "provider driver transparency fixture does not use the expected RFC 6962 single-leaf proof",
    )

    operation = load_json("contracts/v1/fixtures/valid/operation.json")
    provider_operation_ids = [entry["providerOperationId"] for entry in operation["providerOperations"]]
    require(
        len(provider_operation_ids) == len(set(provider_operation_ids)),
        "provider operation canonical IDs are duplicated",
    )

    migration = load_json("contracts/v1/fixtures/valid/identity-migration.json")
    phase_names = [phase["name"] for phase in migration["phases"]]
    require(phase_names == EXPECTED_MIGRATION_PHASES, "migration fixture phases are incomplete or reordered")
    backend_evidence = load_json("contracts/v1/fixtures/valid/backend-migration-evidence.json")
    require(
        backend_evidence.get("authenticationDomain") == "migration-admin",
        "backend migration evidence must use the independent administrator domain",
    )
    require("fenceToken" not in backend_evidence.get("fence", {}), "backend evidence leaked a fence token")
    components = [entry["component"] for entry in backend_evidence.get("components", [])]
    require(len(components) == len(set(components)), "backend evidence repeats a component")
    restore_set = load_json("contracts/v1/fixtures/valid/backend-restore-set.json")
    restore_components = restore_set.get("components", [])
    require(
        [entry.get("backend") for entry in restore_components]
        == ["deis-store", "deis-queue", "deis-telemetry"],
        "backend restore set must contain store, queue and telemetry exactly once in canonical order",
    )
    require(
        not any(entry.get("containsFenceJournal") for entry in restore_components),
        "backend restore set must never copy a migration fence journal",
    )
    require(
        restore_set["consistencyBarrier"].get("kind") == "full-platform-outage",
        "backend restore set must carry a full-platform outage barrier",
    )
    restore_report = load_json("contracts/v1/fixtures/valid/backend-restore-report.json")
    require(
        [entry.get("backend") for entry in restore_report.get("components", [])]
        == ["deis-store", "deis-queue", "deis-telemetry"],
        "backend restore report must use canonical component order",
    )
    require(
        restore_report.get("restoreSetId") == restore_set.get("restoreSetId"),
        "backend restore report fixture must bind the restore set fixture",
    )
    list_page = load_json("contracts/v1/fixtures/valid/control-list-page.json")
    page = list_page.get("page", {})
    items = list_page.get("items", [])
    require(
        list_page.get("schemaVersion") == 1
        and page.get("count") == len(items)
        and page.get("count", -1) <= page.get("limit", -1)
        and page.get("total", -1) >= page.get("count", -1),
        "Control list page fixture violates count, limit, or exact-total invariants",
    )
    restore_set_canonical = json.dumps(
        restore_set, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    require(
        restore_report.get("restoreSetSha256", {}).get("value")
        == hashlib.sha256(restore_set_canonical).hexdigest(),
        "backend restore report fixture must bind canonical restore set bytes",
    )
    for environment in migration["environments"]:
        expected_key = (
            f"v1/{migration['canonicalApplicationId']}/"
            f"{environment['canonicalEnvironmentId']}"
        )
        require(environment["canonicalTenantKey"] == expected_key, "canonical tenant key is not ID-based")
        require(environment["legacyTenantKey"] != expected_key, "legacy and canonical tenant keys unexpectedly match")

    # The driver request grew `rollbackTarget` after drivers were already
    # parsing v1 requests with unknown members rejected, so it must stay an
    # optional member, and the identity it copies from the activate receipt
    # must keep the receipt's own vocabulary.
    process_schema = load_json("contracts/v1/provider-driver-process.schema.json")
    driver_request = process_schema["$defs"]["driverRequest"]
    require(
        "rollbackTarget" in driver_request["properties"]
        and "rollbackTarget" not in driver_request["required"],
        "driver request rollbackTarget must exist and remain optional",
    )
    rollback_target = process_schema["$defs"]["rollbackTarget"]
    require(
        rollback_target.get("additionalProperties") is False
        and set(rollback_target["required"])
        == {"sourceOperationId", "activationOperationId", "activationReceiptSha256"},
        "driver rollback target must pin the source operation and activation receipt only",
    )
    receipt_properties = process_schema["$defs"]["receipt"]["properties"]
    for member in ("providerDeploymentId", "providerVersionId", "providerResourceIds"):
        require(
            rollback_target["properties"].get(member) == receipt_properties.get(member),
            f"driver rollback target {member} must match the receipt member it is copied from",
        )
    rollback_request = load_json(
        "contracts/v1/fixtures/valid/provider-driver-process-rollback-request.json"
    )["request"]
    require(
        rollback_request["operation"] == "rollback"
        and rollback_request["rollbackTarget"]["sourceOperationId"] != rollback_request["operationId"]
        and rollback_request["rollbackTarget"]["activationOperationId"] != rollback_request["operationId"],
        "rollback request fixture must name a different source deployment and activation",
    )


def main() -> int:
    try:
        registry = load_json(REGISTRY_PATH)
        schemas = validate_schemas_and_fixtures(registry)
        validate_operation_transitions(registry, schemas["contracts/v1/operation.schema.json"])
        validate_control_error_invariants(registry)
        validate_cross_document_invariants(registry)
    except (KeyError, IndexError, TypeError, jsonschema.SchemaError, ContractError) as error:
        print(f"shared contract validation failed: {error}", file=sys.stderr)
        return 1

    fixtures = load_json(registry["fixtures"])["cases"]
    print(
        "shared contract validation passed: "
        f"{len(registry['documents'])} schemas, {len(fixtures)} positive/negative fixtures, "
        f"{len(load_json(registry['operationTransitions'])['transitions'])} operation transitions, "
        "identity migration remains explicit"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
