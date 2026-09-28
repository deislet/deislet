#!/usr/bin/env python3
"""Hold contracts/openapi/v1.json to its compatibility promise.

Two things are checked, with the standard library only:

1. The document's own invariants - what docs/adr/016-api-cli-web-console.md
   promises and what the node channel must never be part of. These hold on
   every run, baseline or not.
2. Compatibility against the baseline, which is the file as `origin/main`
   has it. A change a client written against the baseline would notice as
   a breakage is refused unless `info.version` bumps its major. When
   `origin/main` does not carry the file yet there is nothing to compare
   with, and the run passes with a note saying so.

Breaking, unless the major bumps: a removed path, operation or 2xx response;
a removed response property or one that stops being required; a request
property that becomes required; a type, format or enum narrowing; a removed
parameter, one that becomes required or a new one that is required; a
request body that appears as required or becomes required; a security
requirement that is added. Other additions are not breaking.

Range and length bounds are read the same way, and they are why a value the
baseline accepted can start being refused without a single type changing:
lowering `limit`'s `maximum` from 1000 to 500 leaves the parameter an
optional integer, and every caller that asks for a thousand rows starts
getting a 400. So on a *request* a raised `minimum`/`minLength`/`minItems`,
a lowered `maximum`/`maxLength`/`maxItems`, a bound that appears where the
baseline had none, and an added or changed `pattern` are all refused; on a
*response* the mirror is refused, because a reader that sized itself to the
baseline's ceiling is the one hurt when the ceiling rises or goes away.

One kind of type change is deliberately not breaking: a *response* value
whose set of types shrinks to a subset of what the baseline promised - in
practice a member that stopped being nullable, either as `["string", "null"]`
becoming `"string"` or as `oneOf: [null, X]` becoming `X`. A client written
against the baseline already handles every value the new document allows, so
nothing it does can fail; the same narrowing on a *request* is refused,
because there a client may already be sending the value that is now gone.

The comparison exercises itself on a table of small documents before
looking at the real one (`self_check`), so a rule that stops firing is
caught here and not by the client it would have failed.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOCUMENT_PATH = "contracts/openapi/v1.json"
CONTROL_ERROR_SCHEMA = "contracts/v1/control-error.schema.json"
BASELINE_REF = "origin/main"
# Where the pre-release corrections below are reported. Seven other routes
# answer with the same `ProductOperation`, by reference; the comparison
# memoizes on schema content rather than on location, so a shared member is
# compared once and named after whichever route reaches it first. Sorted
# order makes that this one.
_OPERATION_LIST_ROUTE = (
    "/api/v1/apps/{app_name}/environments/{environment_name}/provider-deployments"
)

TAG_PREFIXES = {
    "product": "/api/v1/",
    "platform": "/api/platform/",
}
NEVER_LISTED_PREFIXES = ("/api/nodes", "/api/routing-table")
NEVER_LISTED_SUFFIXES = ("/bundle",)
NEVER_LISTED_EXACT = ("/api/metrics",)
LIST_ENVELOPE_PREFIX = "#/components/schemas/ControlListPage_"
SECRET_NAME = re.compile(r"(?i)(secret|password|private_?key|master_?key|preshared|psk)")
# Schemas that legitimately carry a member named like a credential: the
# metadata of a secret (never its value) and the two answers that hand a
# freshly minted credential to the caller who asked for it. Kept in step with
# the allowlist in crates/deis-control/src/api/openapi.rs.
SECRET_NAME_ALLOWLIST = {
    ("SecretMeta", "name"),
    ("SecretChangeResponse", "secret"),
    ("MintResponse", "secret"),
    ("StorageTokenResponse", "token"),
    ("TelemetryTokenResponse", "token"),
    ("ControlListPage_SecretMeta", "items"),
    # A count of secrets removed with an application.
    ("DeletedApp", "secrets"),
    # Secret revision heads a plan was cut against, and whether they are
    # compared: names and revision ids, never values.
    ("DeploymentPlan", "expected_secret_heads"),
    ("DeploymentPlan", "compare_secret_heads"),
}
METHODS = ("get", "put", "post", "delete", "options", "head", "patch", "trace")


class ContractError(Exception):
    """A rule the document breaks."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ContractError(message)


def load_document() -> dict:
    with (ROOT / DOCUMENT_PATH).open(encoding="utf-8") as handle:
        return json.load(handle)


def load_baseline() -> dict | None:
    """The document as origin/main has it, or None when it has none."""
    result = subprocess.run(
        ["git", "show", f"{BASELINE_REF}:{DOCUMENT_PATH}"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    return json.loads(result.stdout)


# ---------------------------------------------------------------------------
# Invariants of the document on its own.
# ---------------------------------------------------------------------------


def operations(document: dict):
    for path, item in sorted(document.get("paths", {}).items()):
        for method in METHODS:
            if method in item:
                yield path, method, item[method]


def resolve(document: dict, schema: dict) -> dict:
    """Follow `$ref` until a concrete schema; cycles resolve to `{}`."""
    seen = set()
    while isinstance(schema, dict) and "$ref" in schema:
        ref = schema["$ref"]
        if ref in seen:
            return {}
        seen.add(ref)
        require(ref.startswith("#/"), f"external reference {ref} is not allowed")
        node = document
        for part in ref[2:].split("/"):
            node = node[part.replace("~1", "/").replace("~0", "~")]
        schema = node
    return schema if isinstance(schema, dict) else {}


def walk_properties(document: dict, name: str, schema: dict, seen: set, out: list) -> None:
    """Every `(schema, property)` pair reachable inline from `schema`. A
    `$ref` is not followed: the component it names is walked under its own
    name, which is the name the allowlist uses."""
    key = id(schema)
    if key in seen or not isinstance(schema, dict) or "$ref" in schema:
        return
    seen.add(key)
    for prop, inner in schema.get("properties", {}).items():
        out.append((name, prop, inner))
        walk_properties(document, name, inner, seen, out)
    for group in ("allOf", "oneOf", "anyOf"):
        for inner in schema.get(group, []):
            walk_properties(document, name, inner, seen, out)
    if isinstance(schema.get("items"), dict):
        walk_properties(document, name, schema["items"], seen, out)


def looks_like_credential(value) -> bool:
    if isinstance(value, str):
        lower = value.lower()
        if lower.startswith(("deis_", "sk_", "-----begin", "bearer ")):
            return True
        return (
            len(value) >= 32
            and "-" not in value
            and value.replace("_", "").isalnum()
            and any(c.isdigit() for c in value)
            and any(c.isalpha() for c in value)
        )
    if isinstance(value, list):
        return any(looks_like_credential(item) for item in value)
    if isinstance(value, dict):
        return any(looks_like_credential(item) for item in value.values())
    return False


def validate_invariants(document: dict) -> None:
    require(document.get("openapi", "").startswith("3."), "document is not OpenAPI 3")
    version = document.get("info", {}).get("version", "")
    require(re.fullmatch(r"\d+\.\d+\.\d+", version) is not None, f"info.version {version!r} is not semver")

    tags = {tag["name"] for tag in document.get("tags", [])}
    require(tags == set(TAG_PREFIXES), f"tags must be exactly {sorted(TAG_PREFIXES)}, got {sorted(tags)}")

    ids = set()
    for path, method, operation in operations(document):
        where = f"{method.upper()} {path}"
        require(not path.startswith(NEVER_LISTED_PREFIXES), f"{where} is a node route and must not be described")
        require(not path.endswith(NEVER_LISTED_SUFFIXES), f"{where} is the bundle download and must not be described")
        require(path not in NEVER_LISTED_EXACT, f"{where} is the Prometheus exposition and must not be described")
        operation_id = operation.get("operationId")
        require(bool(operation_id), f"{where} has no operationId")
        require(operation_id not in ids, f"operationId {operation_id} is used twice")
        ids.add(operation_id)
        op_tags = operation.get("tags", [])
        require(len(op_tags) == 1, f"{where} must carry exactly one tag")
        require(op_tags[0] in TAG_PREFIXES, f"{where} carries unknown tag {op_tags[0]}")
        require(
            path.startswith(TAG_PREFIXES[op_tags[0]]),
            f"{where} is tagged {op_tags[0]} but does not live under {TAG_PREFIXES[op_tags[0]]}",
        )
        responses = operation.get("responses", {})
        require(any(code.startswith("2") for code in responses), f"{where} documents no success response")
        require(
            any(code[0] in "45" for code in responses),
            f"{where} documents no error response",
        )
        parameters = operation.get("parameters", [])
        takes_cursor = any(p.get("name") == "cursor" and p.get("in") == "query" for p in parameters)
        if method == "get" and takes_cursor:
            schema = responses.get("200", {}).get("content", {}).get("application/json", {}).get("schema", {})
            ref = schema.get("$ref", "")
            require(
                ref.startswith(LIST_ENVELOPE_PREFIX),
                f"{where} takes a cursor but its 200 is {ref or 'not a named schema'}, not a ControlListPage",
            )

    schemas = document.get("components", {}).get("schemas", {})
    found = []
    for name, schema in schemas.items():
        walk_properties(document, name, schema, set(), found)
    offenders = sorted(
        f"{name}.{prop}"
        for name, prop, _ in found
        if SECRET_NAME.search(prop) and (name, prop) not in SECRET_NAME_ALLOWLIST
    )
    require(not offenders, f"properties named like a credential outside the allowlist: {offenders}")

    def scan_examples(node, where: str) -> None:
        if isinstance(node, dict):
            for key in ("example", "examples", "default"):
                if key in node:
                    require(not looks_like_credential(node[key]), f"{where}.{key} looks like a credential")
            for key, inner in node.items():
                scan_examples(inner, f"{where}.{key}")
        elif isinstance(node, list):
            for index, inner in enumerate(node):
                scan_examples(inner, f"{where}[{index}]")

    scan_examples(document, "$")

    error_codes = schemas.get("ErrorCode", {}).get("enum")
    require(error_codes is not None, "components.schemas.ErrorCode must be an enum")
    with (ROOT / CONTROL_ERROR_SCHEMA).open(encoding="utf-8") as handle:
        contract = json.load(handle)
    contract_codes = contract["properties"]["error"]["properties"]["code"]["enum"]
    require(
        set(error_codes) == set(contract_codes),
        f"ErrorCode enum and {CONTROL_ERROR_SCHEMA} disagree: "
        f"{sorted(set(error_codes) ^ set(contract_codes))}",
    )


# ---------------------------------------------------------------------------
# Compatibility against the baseline.
# ---------------------------------------------------------------------------


class Comparison:
    # Keywords that carve a range out of a type. The first group raises the
    # floor a value has to clear, the second lowers the ceiling it has to stay
    # under; an absent keyword is the bound at infinity, so a keyword that
    # appears where the baseline had none moves that bound just as surely as
    # one that changes value.
    LOWER_BOUNDS = ("minimum", "exclusiveMinimum", "minLength", "minItems", "minProperties")
    UPPER_BOUNDS = ("maximum", "exclusiveMaximum", "maxLength", "maxItems", "maxProperties")

    def __init__(self, old: dict, new: dict) -> None:
        self.old = old
        self.new = new
        self.breaking: list[str] = []
        self.visited: set[tuple] = set()

    def note(self, message: str) -> None:
        self.breaking.append(message)

    @staticmethod
    def _type_set(value) -> set:
        return set(value) if isinstance(value, list) else {value}

    @staticmethod
    def _without_null(group: list) -> list:
        return [member for member in group if member != {"type": "null"}]

    def bounds(self, old: dict, new: dict, where: str, direction: str) -> None:
        """Refuse a range a client written against the baseline falls outside
        of: on a request the room to send shrinks, on a response the range to
        read grows past what the reader sized itself for."""
        for keyword in self.LOWER_BOUNDS + self.UPPER_BOUNDS:
            raises_floor = keyword in self.LOWER_BOUNDS
            old_value = old.get(keyword)
            new_value = new.get(keyword)
            if old_value == new_value:
                continue
            if direction == "request":
                # Dropping a bound only ever gives the sender more room.
                if new_value is None:
                    continue
                if old_value is None:
                    self.note(f"{where}: {keyword} {new_value} added where the request had no bound")
                    continue
            else:
                # A response bound the baseline never stated promised nothing.
                if old_value is None:
                    continue
                if new_value is None:
                    self.note(f"{where}: {keyword} {old_value} dropped from the response")
                    continue
            if not self._comparable(old_value, new_value):
                # A bound spelled in some way this comparison cannot order
                # (the 3.0 boolean `exclusiveMinimum` against the 3.1 number,
                # say) is reported rather than assumed harmless.
                self.note(f"{where}: {keyword} changed from {old_value!r} to {new_value!r}")
                continue
            if direction == "request":
                if (new_value > old_value) if raises_floor else (new_value < old_value):
                    self.note(f"{where}: {keyword} narrowed from {old_value} to {new_value}")
            elif (new_value < old_value) if raises_floor else (new_value > old_value):
                self.note(f"{where}: {keyword} widened from {old_value} to {new_value}")
        old_pattern = old.get("pattern")
        new_pattern = new.get("pattern")
        if old_pattern != new_pattern:
            # Whether one regex accepts everything another does is not a
            # question this gate answers, so any pattern that appears or
            # changes on a request, and any that changes or goes away on a
            # response, counts against the promise.
            if direction == "request" and new_pattern is not None:
                self.note(f"{where}: request pattern changed from {old_pattern!r} to {new_pattern!r}")
            elif direction == "response" and old_pattern is not None:
                self.note(f"{where}: response pattern changed from {old_pattern!r} to {new_pattern!r}")

    @staticmethod
    def _comparable(old_value, new_value) -> bool:
        """Both bounds are numbers this comparison can order."""
        return all(
            isinstance(value, (int, float)) and not isinstance(value, bool)
            for value in (old_value, new_value)
        )

    def schema(self, old: dict, new: dict, where: str, direction: str) -> None:
        """`direction` is "request" or "response": what a client sends or
        what it reads, which decides which way a change hurts."""
        key = (json.dumps(old, sort_keys=True), json.dumps(new, sort_keys=True), direction)
        if key in self.visited:
            return
        self.visited.add(key)
        old = resolve(self.old, old)
        new = resolve(self.new, new)
        if not old:
            return
        old_type = old.get("type")
        new_type = new.get("type")
        if old_type is not None and new_type != old_type:
            # A response that can no longer be null (or is otherwise a subset
            # of what was promised) is read correctly by a client written
            # against the wider baseline; the same change on a request takes
            # away a value that client may be sending.
            narrowed = (
                direction == "response"
                and new_type is not None
                and self._type_set(new_type) <= self._type_set(old_type)
            )
            if not narrowed:
                self.note(f"{where}: type changed from {old_type} to {new_type}")
        if old.get("format") is not None and new.get("format") != old.get("format"):
            self.note(f"{where}: format changed from {old.get('format')} to {new.get('format')}")
        if "enum" in old:
            removed = sorted(set(map(str, old["enum"])) - set(map(str, new.get("enum", []))))
            if removed:
                self.note(f"{where}: enum values removed: {removed}")
        self.bounds(old, new, where, direction)
        old_props = old.get("properties", {})
        new_props = new.get("properties", {})
        old_required = set(old.get("required", []))
        new_required = set(new.get("required", []))
        for name, inner in old_props.items():
            if name not in new_props:
                self.note(f"{where}: {direction} property {name} removed")
                continue
            self.schema(inner, new_props[name], f"{where}.{name}", direction)
        if direction == "response":
            for name in sorted(old_required - new_required):
                if name in new_props:
                    self.note(f"{where}: response property {name} is no longer required")
        else:
            for name in sorted(new_required - old_required):
                self.note(f"{where}: request property {name} became required")
        if isinstance(old.get("items"), dict):
            self.schema(old["items"], new.get("items", {}), f"{where}[]", direction)
        for group in ("allOf", "oneOf", "anyOf"):
            old_group = old.get(group, [])
            new_group = new.get(group, [])
            # `oneOf: [null, X]` for a response member that stopped being
            # nullable renders as X itself, with no group left to walk. The
            # one remaining member is compared against what took its place.
            if (
                direction == "response"
                and group in ("oneOf", "anyOf")
                and old_group
                and not new_group
            ):
                remaining = self._without_null(old_group)
                if len(remaining) == 1 and len(remaining) < len(old_group):
                    self.schema(remaining[0], new, f"{where}.{group}[0]", direction)
                    continue
            if old_group and len(new_group) < len(old_group):
                self.note(f"{where}: {group} lost members")
                continue
            for index, inner in enumerate(old_group):
                self.schema(inner, new_group[index], f"{where}.{group}[{index}]", direction)

    def content(self, old: dict, new: dict, where: str, direction: str) -> None:
        for media, old_media in old.items():
            if media not in new:
                self.note(f"{where}: {media} removed")
                continue
            self.schema(old_media.get("schema", {}), new[media].get("schema", {}), f"{where} {media}", direction)

    def parameters(self, old: list, new: list, where: str) -> None:
        old_by_key = {(p.get("in"), p.get("name")): p for p in old}
        new_by_key = {(p.get("in"), p.get("name")): p for p in new}
        for key, parameter in old_by_key.items():
            current = new_by_key.get(key)
            if current is None:
                self.note(f"{where}: parameter {key[1]} ({key[0]}) removed")
                continue
            if current.get("required", False) and not parameter.get("required", False):
                self.note(f"{where}: parameter {key[1]} ({key[0]}) became required")
            self.schema(parameter.get("schema", {}), current.get("schema", {}), f"{where} parameter {key[1]}", "request")
        # A parameter the baseline never had is fine to add - unless a client
        # written against the baseline now has to send it. A path parameter
        # is always required, and a new one means a new path, caught above.
        for key, parameter in new_by_key.items():
            if key not in old_by_key and parameter.get("required", False):
                self.note(f"{where}: new parameter {key[1]} ({key[0]}) is required")

    def operation(self, old: dict, new: dict, where: str) -> None:
        self.parameters(old.get("parameters", []), new.get("parameters", []), where)
        old_body = old.get("requestBody", {})
        new_body = new.get("requestBody", {})
        if old_body:
            if not new_body:
                self.note(f"{where}: request body removed")
            else:
                if new_body.get("required", False) and not old_body.get("required", False):
                    self.note(f"{where}: request body became required")
                self.content(old_body.get("content", {}), new_body.get("content", {}), f"{where} request", "request")
        elif new_body.get("required", False):
            # The baseline took no body; a client written against it sends
            # none, and now must.
            self.note(f"{where}: request body became required")
        old_responses = old.get("responses", {})
        new_responses = new.get("responses", {})
        for code, old_response in old_responses.items():
            if code not in new_responses:
                if code.startswith("2"):
                    self.note(f"{where}: {code} response removed")
                continue
            old_response = resolve(self.old, old_response)
            new_response = resolve(self.new, new_responses[code])
            self.content(old_response.get("content", {}), new_response.get("content", {}), f"{where} {code}", "response")
            for header in old_response.get("headers", {}):
                if header not in new_response.get("headers", {}):
                    self.note(f"{where} {code}: header {header} removed")
        old_security = old.get("security")
        new_security = new.get("security")
        if old_security is None and new_security:
            self.note(f"{where}: a security requirement was added")

    def run(self) -> None:
        old_ops = {(path, method): op for path, method, op in operations(self.old)}
        new_ops = {(path, method): op for path, method, op in operations(self.new)}
        for (path, method), old_op in sorted(old_ops.items()):
            where = f"{method.upper()} {path}"
            new_op = new_ops.get((path, method))
            if new_op is None:
                self.note(f"{where}: operation removed")
                continue
            if old_op.get("operationId") != new_op.get("operationId"):
                self.note(f"{where}: operationId changed from {old_op.get('operationId')} to {new_op.get('operationId')}")
            self.operation(old_op, new_op, where)
        if not self.old.get("security") and self.new.get("security"):
            self.note("a global security requirement was added")


def major(version: str) -> int:
    return int(version.split(".")[0])


# ---------------------------------------------------------------------------
# The comparison's own check: one small document per rule, mutated the way a
# careless change would, and the phrase the rule must answer with. A mutation
# listed with `None` must pass as additive.
# ---------------------------------------------------------------------------


def _minimal_document() -> dict:
    return {
        "openapi": "3.1.0",
        "info": {"version": "1.0.0"},
        "paths": {
            "/api/v1/apps": {
                "get": {
                    "operationId": "listApps",
                    "parameters": [
                        {"name": "limit", "in": "query", "required": False, "schema": {"type": "integer"}},
                    ],
                    "responses": {
                        "200": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["items"],
                                        "properties": {
                                            "items": {"type": "array", "items": {"type": "string"}},
                                            "total": {"type": "integer"},
                                        },
                                    }
                                }
                            }
                        }
                    },
                },
                "post": {
                    "operationId": "createApp",
                    "requestBody": {
                        "required": True,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["id"],
                                    "properties": {"id": {"type": "string"}, "note": {"type": "string"}},
                                }
                            }
                        },
                    },
                    "responses": {"201": {}},
                },
                "delete": {"operationId": "deleteApps", "responses": {"204": {}}},
            }
        },
    }


def _mutated(mutate) -> dict:
    document = _minimal_document()
    mutate(document)
    return document


SELF_CHECK_CASES = [
    ("a removed operation", lambda d: d["paths"]["/api/v1/apps"].pop("delete"), "operation removed"),
    ("a removed parameter", lambda d: d["paths"]["/api/v1/apps"]["get"].__setitem__("parameters", []), "parameter limit (query) removed"),
    ("a parameter that became required", lambda d: d["paths"]["/api/v1/apps"]["get"]["parameters"][0].__setitem__("required", True), "parameter limit (query) became required"),
    ("a new required parameter", lambda d: d["paths"]["/api/v1/apps"]["get"]["parameters"].append({"name": "x-tenant", "in": "header", "required": True, "schema": {"type": "string"}}), "new parameter x-tenant (header) is required"),
    ("a new optional parameter", lambda d: d["paths"]["/api/v1/apps"]["get"]["parameters"].append({"name": "cursor", "in": "query", "required": False, "schema": {"type": "string"}}), None),
    ("a request body that appeared as required", lambda d: d["paths"]["/api/v1/apps"]["delete"].__setitem__("requestBody", {"required": True, "content": {"application/json": {"schema": {"type": "object"}}}}), "request body became required"),
    ("a request body that appeared as optional", lambda d: d["paths"]["/api/v1/apps"]["delete"].__setitem__("requestBody", {"required": False, "content": {"application/json": {"schema": {"type": "object"}}}}), None),
    ("a request body that became required", lambda d: _optional_then_required(d), "request body became required"),
    ("a request property that became required", lambda d: d["paths"]["/api/v1/apps"]["post"]["requestBody"]["content"]["application/json"]["schema"].__setitem__("required", ["id", "note"]), "request property note became required"),
    ("a removed response property", lambda d: d["paths"]["/api/v1/apps"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]["properties"].pop("total"), "response property total removed"),
    ("a response property no longer required", lambda d: d["paths"]["/api/v1/apps"]["get"]["responses"]["200"]["content"]["application/json"]["schema"].__setitem__("required", []), "response property items is no longer required"),
    ("a changed type", lambda d: d["paths"]["/api/v1/apps"]["get"]["parameters"][0]["schema"].__setitem__("type", "string"), "type changed from integer to string"),
    ("a removed 2xx response", lambda d: d["paths"]["/api/v1/apps"]["post"]["responses"].pop("201"), "201 response removed"),
    ("an added security requirement", lambda d: d["paths"]["/api/v1/apps"]["get"].__setitem__("security", [{"bearer": []}]), "a security requirement was added"),
    ("an added response property", lambda d: d["paths"]["/api/v1/apps"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]["properties"].__setitem__("page", {"type": "integer"}), None),
]


def _nullable(schema: dict) -> dict:
    """The same schema, spelled as it is when the member may also be null."""
    return {"type": [schema["type"], "null"]}


def _response_property(document: dict, name: str) -> dict:
    return document["paths"]["/api/v1/apps"]["get"]["responses"]["200"]["content"][
        "application/json"
    ]["schema"]["properties"][name]


def _request_property(document: dict, name: str) -> dict:
    return document["paths"]["/api/v1/apps"]["post"]["requestBody"]["content"]["application/json"][
        "schema"
    ]["properties"][name]


def _set_response_property(document: dict, name: str, schema: dict) -> None:
    document["paths"]["/api/v1/apps"]["get"]["responses"]["200"]["content"]["application/json"][
        "schema"
    ]["properties"][name] = schema


def _set_request_property(document: dict, name: str, schema: dict) -> None:
    document["paths"]["/api/v1/apps"]["post"]["requestBody"]["content"]["application/json"][
        "schema"
    ]["properties"][name] = schema


# Nullability, both ways round: the baseline is mutated too, because these
# cases are about what a member *was*. A response that stops being nullable is
# read by every client written against the baseline; a request that stops
# accepting null takes away a value a client may already be sending, and a
# response that starts being nullable hands a reader a value its types do not
# admit.
NULLABILITY_CASES = [
    (
        "a response member that stopped being nullable",
        lambda d: _set_response_property(d, "total", _nullable(_response_property(d, "total"))),
        lambda d: _set_response_property(d, "total", {"type": "integer"}),
        None,
    ),
    (
        "a response member that became nullable",
        lambda d: None,
        lambda d: _set_response_property(d, "total", {"type": ["integer", "null"]}),
        "type changed from integer to ['integer', 'null']",
    ),
    (
        "a request member that stopped being nullable",
        lambda d: _set_request_property(d, "note", _nullable(_request_property(d, "note"))),
        lambda d: _set_request_property(d, "note", {"type": "string"}),
        "type changed from ['string', 'null'] to string",
    ),
    (
        "a response oneOf that lost only its null",
        lambda d: _set_response_property(
            d, "total", {"oneOf": [{"type": "null"}, {"type": "integer"}]}
        ),
        lambda d: _set_response_property(d, "total", {"type": "integer"}),
        None,
    ),
    (
        "a response oneOf that lost the member carrying the value",
        lambda d: _set_response_property(
            d, "total", {"oneOf": [{"type": "null"}, {"type": "integer"}]}
        ),
        lambda d: _set_response_property(d, "total", {"type": "null"}),
        "type changed from integer to null",
    ),
]


def _parameter_schema(document: dict) -> dict:
    return document["paths"]["/api/v1/apps"]["get"]["parameters"][0]["schema"]


# Bounds, both ways round, on the same footing as nullability: the baseline is
# mutated too, because these cases are about the room a value already had. The
# first one is the change that prompted the rule - `limit` keeps its type and
# its optionality, and every caller asking for a thousand rows starts getting
# a 400 anyway.
BOUNDS_CASES = [
    (
        "a request parameter whose maximum was lowered",
        lambda d: _parameter_schema(d).update({"minimum": 1, "maximum": 1000}),
        lambda d: _parameter_schema(d).__setitem__("maximum", 500),
        "maximum narrowed from 1000 to 500",
    ),
    (
        "a request parameter that gained a maximum",
        lambda d: None,
        lambda d: _parameter_schema(d).__setitem__("maximum", 500),
        "maximum 500 added where the request had no bound",
    ),
    (
        "a request parameter whose maximum was raised",
        lambda d: _parameter_schema(d).__setitem__("maximum", 1000),
        lambda d: _parameter_schema(d).__setitem__("maximum", 2000),
        None,
    ),
    (
        "a request parameter whose minimum was raised",
        lambda d: _parameter_schema(d).__setitem__("minimum", 1),
        lambda d: _parameter_schema(d).__setitem__("minimum", 2),
        "minimum narrowed from 1 to 2",
    ),
    (
        "a request parameter that dropped its minimum",
        lambda d: _parameter_schema(d).__setitem__("minimum", 1),
        lambda d: _parameter_schema(d).pop("minimum"),
        None,
    ),
    (
        "a request property whose maxLength shrank",
        lambda d: _request_property(d, "note").__setitem__("maxLength", 64),
        lambda d: _request_property(d, "note").__setitem__("maxLength", 32),
        "maxLength narrowed from 64 to 32",
    ),
    (
        "a request property that gained a pattern",
        lambda d: None,
        lambda d: _request_property(d, "note").__setitem__("pattern", "^[a-z]+$"),
        "request pattern changed",
    ),
    (
        "a response property whose maximum was raised",
        lambda d: _response_property(d, "total").__setitem__("maximum", 100),
        lambda d: _response_property(d, "total").__setitem__("maximum", 1000),
        "maximum widened from 100 to 1000",
    ),
    (
        "a response property that dropped its maxItems",
        lambda d: _response_property(d, "items").__setitem__("maxItems", 100),
        lambda d: _response_property(d, "items").pop("maxItems"),
        "maxItems 100 dropped from the response",
    ),
    (
        "a response property whose maximum was lowered",
        lambda d: _response_property(d, "total").__setitem__("maximum", 100),
        lambda d: _response_property(d, "total").__setitem__("maximum", 50),
        None,
    ),
]


def _optional_then_required(document: dict) -> None:
    """The baseline for this case takes an optional body; see `self_check`."""
    document["paths"]["/api/v1/apps"]["post"]["requestBody"]["required"] = True


def self_check() -> None:
    cases = [(label, None, mutate, expected) for label, mutate, expected in SELF_CHECK_CASES]
    cases.extend(NULLABILITY_CASES)
    cases.extend(BOUNDS_CASES)
    for label, mutate_baseline, mutate, expected in cases:
        baseline = _minimal_document()
        if label == "a request body that became required":
            baseline["paths"]["/api/v1/apps"]["post"]["requestBody"]["required"] = False
        if mutate_baseline is not None:
            mutate_baseline(baseline)
        document = _minimal_document()
        if mutate_baseline is not None:
            mutate_baseline(document)
        mutate(document)
        comparison = Comparison(baseline, document)
        comparison.run()
        if expected is None:
            require(not comparison.breaking, f"self-check: {label} was reported as breaking: {comparison.breaking}")
            continue
        require(
            any(expected in line for line in comparison.breaking),
            f"self-check: {label} was not refused (got {comparison.breaking})",
        )


# Changes accepted once, by name, because v1 has not been published.
#
# There is no client anywhere that was written against the baseline, so the
# question the rules above answer - what would a reader already out there
# notice - has no subject yet. That is the only thing this list stands on,
# and it goes away when v1 ships.
#
# Two kinds of entry have been here.
#
# The first was a response member that really is `null` on the wire and was
# documented as though it never could be. Those eight are on `origin/main`
# now, so the baseline carries them and the comparison no longer reports
# them; they have been deleted, which is what this list is for.
#
# The second is what is here now: a response member that described a
# credential this platform no longer has. The fleet-wide node key is gone -
# it is not a field of any configuration struct, and a control plane whose
# file still sets `node_preshared_keys` refuses to start - so "which of the
# fleet's keys did this node prove" has no answer to report. Restoring the
# members is not available; they would have to be filled with something
# untrue. The same holds for the flat configuration-file API key: its
# origin, `config`, is no longer a value the platform can answer with, so
# the enum loses it. Removing a response property, or a value a response
# enum may take, is refused on principle, so each removal is named here in
# full: an entry that does not match a reported change exactly does
# nothing, and a removal that is not listed is still refused.
#
# The list is meant to be emptied rather than grown. Once these are on
# `origin/main` the baseline carries them and the comparison stops reporting
# them, which the run says so that the entries can be deleted.
PRERELEASE_CORRECTIONS = frozenset(
    {
        "POST /api/v1/accounts/{account}/tokens 200 application/json.token.origin: "
        "enum values removed: ['config']",
    }
)


def main() -> int:
    try:
        self_check()
        document = load_document()
        validate_invariants(document)
    except (OSError, KeyError, TypeError, ValueError, ContractError) as error:
        print(f"openapi contract check failed: {error}", file=sys.stderr)
        return 1

    baseline = load_baseline()
    count = sum(1 for _ in operations(document))
    if baseline is None:
        print(
            f"openapi contract check passed: {count} operations hold the document invariants; "
            f"{BASELINE_REF} carries no {DOCUMENT_PATH} yet, so there is no baseline to compare with"
        )
        return 0

    try:
        validate_invariants(baseline)
    except ContractError as error:
        print(f"note: the baseline on {BASELINE_REF} breaks a current invariant ({error}); comparing anyway")

    comparison = Comparison(baseline, document)
    comparison.run()
    old_version = baseline["info"]["version"]
    new_version = document["info"]["version"]

    accepted = [line for line in comparison.breaking if line in PRERELEASE_CORRECTIONS]
    comparison.breaking = [
        line for line in comparison.breaking if line not in PRERELEASE_CORRECTIONS
    ]
    if accepted:
        print(
            f"note: {len(accepted)} pre-release correction(s) accepted "
            f"(see PRERELEASE_CORRECTIONS in {Path(__file__).name}):"
        )
        for line in accepted:
            print(f"  - {line}")
    stale = sorted(PRERELEASE_CORRECTIONS - set(accepted))
    if stale:
        print(
            f"note: {len(stale)} pre-release correction(s) are no longer reported and can be "
            f"removed from the list:"
        )
        for line in stale:
            print(f"  - {line}")

    if comparison.breaking:
        if major(new_version) > major(old_version):
            print(
                f"openapi contract check passed: {len(comparison.breaking)} breaking change(s) "
                f"accepted by the major bump {old_version} -> {new_version}:"
            )
            for line in comparison.breaking:
                print(f"  - {line}")
            return 0
        print(
            f"openapi contract check failed: {len(comparison.breaking)} breaking change(s) against "
            f"{BASELINE_REF} and info.version stays at major {major(new_version)} "
            f"({old_version} -> {new_version}):",
            file=sys.stderr,
        )
        for line in comparison.breaking:
            print(f"  - {line}", file=sys.stderr)
        print(
            "Either restore what the baseline promised, or bump the major in "
            "crates/deis-control/src/api/openapi.rs (DOCUMENT_VERSION) and regenerate.",
            file=sys.stderr,
        )
        return 1

    if baseline == document:
        print(f"openapi contract check passed: {count} operations, unchanged from {BASELINE_REF}")
    else:
        print(
            f"openapi contract check passed: {count} operations, additive changes only "
            f"({old_version} -> {new_version})"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
