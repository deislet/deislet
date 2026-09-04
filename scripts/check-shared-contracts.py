#!/usr/bin/env python3
"""Execute the draft v1 shared-model schemas and cross-document invariants."""

from __future__ import annotations

import copy
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
    "identity-migration",
    "backend-migration-evidence",
    "backend-restore-set",
    "backend-restore-report",
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
    require(artifact.get("deployable") is False, "proposal artifact fixture must remain non-deployable")

    binding = load_json("contracts/v1/fixtures/valid/binding-snapshot.json")
    declarations = {entry["bindingId"]: (entry["name"], entry["kind"]) for entry in binding["declarations"]}
    resolutions = {entry["bindingId"]: (entry["name"], entry["kind"]) for entry in binding["resolutions"]}
    require(declarations == resolutions, "binding declarations and resolutions differ")

    driver = load_json("contracts/v1/fixtures/valid/provider-driver.json")
    require(driver.get("operations") == EXPECTED_PROVIDER_OPERATIONS, "provider fixture omits a required operation")
    require(driver.get("supportedStandardVersions") == [], "proposal driver must not claim a supported standard version")

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


def main() -> int:
    try:
        registry = load_json(REGISTRY_PATH)
        schemas = validate_schemas_and_fixtures(registry)
        validate_operation_transitions(registry, schemas["contracts/v1/operation.schema.json"])
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
