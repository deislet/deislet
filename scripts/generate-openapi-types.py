#!/usr/bin/env python3
"""Render contracts/openapi/v1.json as the web console's TypeScript types.

The console used to carry its own copy of every response model it read:
an interface per shape, written by hand from the Rust struct, and kept in
step with it by nobody. Fields that the server had never sent
(`nodes.unresponsive`), enums narrower than the wire (`state`, `status`),
and members the console did not know the server had (`key_status`,
`pagination`) were all found in that copy. docs/adr/016-api-cli-web-console.md
rules it out: the TypeScript client is generated from the shared schema or
checked against it in CI, thin domain wrappers are allowed, hand-written
copies of response models are not.

This script is the generator. It reads the committed OpenAPI document with
the standard library only and writes one TypeScript module,
`crates/deis-control/web/src/generated/control-api.ts`:

* one exported type per `components.schemas` entry, under the name the
  document uses (`FleetResponse`, `ControlListPage_Account`, ...), with the
  description as JSDoc on the type and on every property;
* a `components` interface that indexes those names the way generated
  OpenAPI clients usually do (`components["schemas"]["FleetResponse"]`);
* an `operations` interface keyed by `operationId`, each carrying the
  method, the path, the path/query/header parameters, the request body and
  the responses by status, so a caller writes
  `request<operations["listRuntimeFleet"]["responses"][200]>(...)` and the
  type checker holds the console to the document.

The emitter understands exactly the JSON Schema subset the document uses:
`$ref` into `components.schemas`, objects with `properties`, `required` and
`additionalProperties`, arrays, the four primitives, `type` given as an
array including `"null"`, `enum` over strings, `oneOf` (a union), `allOf`
(an intersection) and `format` (kept as a comment, never widening or
narrowing the type). Any other construct stops the run with the path to it,
so a new keyword in the document turns into a decision here and not into an
`unknown` that a page later reads a field out of.

Modes:

    python3 scripts/generate-openapi-types.py          # rewrite the module
    python3 scripts/generate-openapi-types.py --check  # exit 1 with a diff
                                                       # when the committed
                                                       # module is stale

CI runs `--check` in the product-governance job, beside
`check-openapi-compat.py`, so a document that changed without its client
being regenerated cannot merge. The emitter exercises itself on a small
inline document before touching the real one (`self_check`), so a rendering
that drifts is caught here and not by `tsc` in the console.
"""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOCUMENT_PATH = "contracts/openapi/v1.json"
OUTPUT_PATH = "crates/deis-control/web/src/generated/control-api.ts"
SCRIPT_PATH = "scripts/generate-openapi-types.py"
SCHEMA_PREFIX = "#/components/schemas/"
RESPONSE_PREFIX = "#/components/responses/"
METHODS = ("get", "put", "post", "delete", "options", "head", "patch", "trace")
INDENT = "  "

# Keywords the emitter renders. Everything the document may say about a
# schema is either here, in `IGNORED_KEYS` because it constrains a value
# without changing its TypeScript type, or a reason to stop.
RENDERED_KEYS = {
    "$ref",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "oneOf",
    "allOf",
    "format",
    "description",
    "deprecated",
    "propertyNames",
}
IGNORED_KEYS = {
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "multipleOf",
    "title",
    "example",
    "examples",
    "default",
    "readOnly",
    "writeOnly",
}
PRIMITIVES = {
    "string": "string",
    "integer": "number",
    "number": "number",
    "boolean": "boolean",
    "null": "null",
}
IDENTIFIER = re.compile(r"[A-Za-z_$][A-Za-z0-9_$]*")


class GeneratorError(Exception):
    """A construct the emitter does not render, with where it was found."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise GeneratorError(message)


def load_document(path: Path) -> tuple[dict, str]:
    """The parsed document and the SHA-256 of the exact bytes it was read
    from, which is what the header of the module records."""
    raw = path.read_bytes()
    return json.loads(raw.decode("utf-8")), hashlib.sha256(raw).hexdigest()


# ---------------------------------------------------------------------------
# JSDoc.
# ---------------------------------------------------------------------------


def jsdoc(description: str | None, indent: str, deprecated: bool = False) -> list[str]:
    """The comment lines for a description, or none when there is nothing
    to say. A `*/` inside a description would end the comment early, so it
    is broken up."""
    lines = []
    if description:
        lines.extend(line.rstrip() for line in description.replace("*/", "*\\/").strip().split("\n"))
    if deprecated:
        lines.append("@deprecated")
    if not lines:
        return []
    if len(lines) == 1:
        return [f"{indent}/** {lines[0]} */"]
    out = [f"{indent}/**"]
    out.extend(f"{indent} * {line}".rstrip() for line in lines)
    out.append(f"{indent} */")
    return out


def property_key(name: str) -> str:
    return name if IDENTIFIER.fullmatch(name) else json.dumps(name)


def has_top_level_operator(rendered: str) -> bool:
    """Whether a rendered type joins members with `|` or `&` outside every
    brace, bracket, parenthesis, string literal and `/* */` comment. Such a
    rendering binds looser than an appended `& ...`, so it needs parentheses
    there; an object literal, an array, a name or a string literal does not.

    A scan, not a pattern: `{x} | {y}` starts with a brace and ends with one
    and is still a union, and `A | B[]` ends in `[]` and is still a union.
    """
    depth = 0
    index = 0
    while index < len(rendered):
        char = rendered[index]
        if char == '"':
            # A string literal: skip to its closing quote, honouring escapes.
            index += 1
            while index < len(rendered) and rendered[index] != '"':
                index += 2 if rendered[index] == "\\" else 1
        elif rendered.startswith("/*", index):
            index = rendered.find("*/", index + 2)
            require(index >= 0, f"unterminated comment in rendered type {rendered!r}")
            index += 1
        elif char in "{[(<":
            depth += 1
        elif char in "}])>":
            depth -= 1
        elif depth == 0 and char in "|&":
            return True
        index += 1
    return False


def parenthesized(rendered: str) -> str:
    return f"({rendered})" if has_top_level_operator(rendered) else rendered


# ---------------------------------------------------------------------------
# Schemas to types.
# ---------------------------------------------------------------------------


class Emitter:
    def __init__(self, document: dict) -> None:
        self.document = document
        self.schemas: dict = document.get("components", {}).get("schemas", {})
        self.responses: dict = document.get("components", {}).get("responses", {})

    # -- schemas ----------------------------------------------------------

    def render_type(self, schema: dict, where: str, indent: str) -> str:
        """The TypeScript type of `schema`. Multi-line renderings (objects
        with properties) are indented so that their closing brace sits at
        `indent`."""
        require(isinstance(schema, dict), f"{where}: schema is not an object")
        unknown = set(schema) - RENDERED_KEYS - IGNORED_KEYS
        require(not unknown, f"{where}: keyword(s) {sorted(unknown)} are not rendered by this generator")

        if "$ref" in schema:
            # In OpenAPI 3.1 a `$ref` may sit beside a description, which the
            # property's JSDoc carries; anything else beside it would be a
            # constraint this emitter silently dropped.
            extra = set(schema) - {"$ref", "description", "deprecated"}
            require(not extra, f"{where}: $ref carries sibling keyword(s) {sorted(extra)}")
            return self.reference(schema["$ref"], where)

        if "oneOf" in schema:
            require(
                set(schema) <= {"oneOf", "description", "deprecated"},
                f"{where}: oneOf mixed with other keywords",
            )
            members = [
                self.render_type(member, f"{where}.oneOf[{index}]", indent)
                for index, member in enumerate(schema["oneOf"])
            ]
            require(members, f"{where}: oneOf has no members")
            return " | ".join(members)

        if "allOf" in schema:
            require(
                set(schema) <= {"allOf", "description", "deprecated"},
                f"{where}: allOf mixed with other keywords",
            )
            members = [
                parenthesized(self.render_type(member, f"{where}.allOf[{index}]", indent))
                for index, member in enumerate(schema["allOf"])
            ]
            require(members, f"{where}: allOf has no members")
            return " & ".join(members)

        if "enum" in schema:
            require(schema.get("type") == "string", f"{where}: enum is only rendered over strings")
            values = schema["enum"]
            require(values and all(isinstance(v, str) for v in values), f"{where}: enum values are not strings")
            return " | ".join(json.dumps(value) for value in values)

        declared = schema.get("type")
        if declared is None:
            # `{}` and `{"description": ...}`: any JSON value at all, which is
            # what `serde_json::Value` renders as.
            leftover = set(schema) - {"description", "deprecated"} - IGNORED_KEYS
            require(not leftover, f"{where}: keyword(s) {sorted(leftover)} without a type")
            return "unknown"

        nullable = False
        if isinstance(declared, list):
            require(
                len(declared) == 2 and "null" in declared,
                f"{where}: type {declared} is rendered only as one type plus null",
            )
            nullable = True
            declared = next(t for t in declared if t != "null")
        require(isinstance(declared, str), f"{where}: type {declared!r} is not a string")

        if declared in PRIMITIVES:
            rendered = PRIMITIVES[declared]
            if "format" in schema:
                rendered = f"{rendered} /* {schema['format']} */"
        elif declared == "array":
            require("items" in schema, f"{where}: array without items")
            item = self.render_type(schema["items"], f"{where}.items", indent)
            rendered = f"{item}[]" if IDENTIFIER.fullmatch(item) else f"Array<{item}>"
        elif declared == "object":
            rendered = self.render_object(schema, where, indent)
        else:
            raise GeneratorError(f"{where}: type {declared!r} is not rendered by this generator")
        return f"{rendered} | null" if nullable else rendered

    def render_object(self, schema: dict, where: str, indent: str) -> str:
        properties = schema.get("properties")
        additional = schema.get("additionalProperties")
        if "propertyNames" in schema:
            require(
                schema["propertyNames"] == {"type": "string"},
                f"{where}: propertyNames other than a plain string is not rendered",
            )
        if properties is None:
            if additional is False:
                return "Record<string, never>"
            if additional is None or additional is True or additional == {}:
                return "{ [key: string]: unknown }"
            value = self.render_type(additional, f"{where}.additionalProperties", indent)
            return f"{{ [key: string]: {value} }}"
        require(
            additional is None or additional is False,
            f"{where}: properties together with an additionalProperties schema is not rendered",
        )
        required = set(schema.get("required", []))
        missing = required - set(properties)
        require(not missing, f"{where}: required names {sorted(missing)} that are not properties")
        inner = indent + INDENT
        lines = ["{"]
        for name, member in properties.items():
            require(isinstance(member, dict), f"{where}.{name}: property schema is not an object")
            lines.extend(jsdoc(member.get("description"), inner, member.get("deprecated", False)))
            rendered = self.render_type(member, f"{where}.{name}", inner)
            marker = "" if name in required else "?"
            lines.append(f"{inner}{property_key(name)}{marker}: {rendered};")
        lines.append(f"{indent}}}")
        return "\n".join(lines)

    def reference(self, ref: str, where: str) -> str:
        require(ref.startswith(SCHEMA_PREFIX), f"{where}: $ref {ref} does not point into components.schemas")
        name = ref[len(SCHEMA_PREFIX) :]
        require(name in self.schemas, f"{where}: $ref {ref} names a schema the document does not define")
        require(IDENTIFIER.fullmatch(name) is not None, f"{where}: schema name {name!r} is not a TypeScript identifier")
        return name

    def schema_declarations(self) -> list[str]:
        out = []
        for name, schema in self.schemas.items():
            require(IDENTIFIER.fullmatch(name) is not None, f"schema name {name!r} is not a TypeScript identifier")
            out.extend(jsdoc(schema.get("description"), "", schema.get("deprecated", False)))
            out.append(f"export type {name} = {self.render_type(schema, name, '')};")
            out.append("")
        return out

    def components_declaration(self) -> list[str]:
        out = [
            "/**",
            " * The schemas by the name the document gives them, for callers that",
            ' * spell a type as `components["schemas"]["Name"]`.',
            " */",
            "export interface components {",
            f"{INDENT}schemas: {{",
        ]
        out.extend(f"{INDENT * 2}{name}: {name};" for name in self.schemas)
        out.append(f"{INDENT}}};")
        out.append("}")
        out.append("")
        return out

    # -- operations -------------------------------------------------------

    def operations(self):
        for path, item in self.document.get("paths", {}).items():
            require(isinstance(item, dict), f"paths.{path}: not an object")
            extra = set(item) - set(METHODS)
            require(not extra, f"paths.{path}: {sorted(extra)} are not rendered (path-level keys)")
            for method in METHODS:
                if method in item:
                    yield path, method, item[method]

    def parameter_group(self, parameters: list, location: str, where: str, indent: str) -> str:
        chosen = [p for p in parameters if p.get("in") == location]
        if not chosen:
            return "never"
        inner = indent + INDENT
        lines = ["{"]
        for parameter in chosen:
            name = parameter.get("name")
            require(isinstance(name, str) and name, f"{where}: a {location} parameter has no name")
            extra = set(parameter) - {"name", "in", "required", "description", "schema", "deprecated"}
            require(not extra, f"{where} parameter {name}: keyword(s) {sorted(extra)} are not rendered")
            require("schema" in parameter, f"{where} parameter {name}: no schema")
            lines.extend(jsdoc(parameter.get("description"), inner, parameter.get("deprecated", False)))
            rendered = self.render_type(parameter["schema"], f"{where} parameter {name}", inner)
            marker = "" if parameter.get("required", False) or location == "path" else "?"
            lines.append(f"{inner}{property_key(name)}{marker}: {rendered};")
        lines.append(f"{indent}}}")
        return "\n".join(lines)

    def request_body(self, body: dict | None, where: str, indent: str) -> str:
        if not body:
            return "never"
        extra = set(body) - {"content", "required", "description"}
        require(not extra, f"{where} request body: keyword(s) {sorted(extra)} are not rendered")
        content = body.get("content", {})
        require(len(content) == 1, f"{where} request body: exactly one media type is rendered, got {sorted(content)}")
        (media, item), = content.items()
        require("schema" in item, f"{where} request body: {media} has no schema")
        rendered = self.render_type(item["schema"], f"{where} request body", indent)
        if media != "application/json":
            rendered = f"{rendered} /* {media} */"
        if not body.get("required", False):
            rendered = f"{rendered} | undefined"
        return rendered

    def response_type(self, response: dict, where: str, indent: str) -> str:
        if "$ref" in response:
            ref = response["$ref"]
            require(set(response) == {"$ref"}, f"{where}: response $ref carries sibling keywords")
            require(ref.startswith(RESPONSE_PREFIX), f"{where}: response $ref {ref} does not point into components.responses")
            name = ref[len(RESPONSE_PREFIX) :]
            require(name in self.responses, f"{where}: response $ref {ref} names a response the document does not define")
            response = self.responses[name]
        extra = set(response) - {"description", "content", "headers"}
        require(not extra, f"{where}: response keyword(s) {sorted(extra)} are not rendered")
        content = response.get("content")
        if not content:
            return "void"
        require(
            set(content) == {"application/json"},
            f"{where}: only an application/json response is rendered, got {sorted(content)}",
        )
        item = content["application/json"]
        require("schema" in item, f"{where}: application/json response has no schema")
        return self.render_type(item["schema"], f"{where} response", indent)

    def operations_declaration(self) -> list[str]:
        out = [
            "/**",
            " * Every operation, by `operationId`. `responses` is keyed by status code, so",
            ' * `operations["listRuntimeFleet"]["responses"][200]` is what a 200 carries;',
            " * `never` marks a parameter group or body the operation does not take.",
            " */",
            "export interface operations {",
        ]
        seen = set()
        for path, method, operation in self.operations():
            where = f"{method.upper()} {path}"
            operation_id = operation.get("operationId")
            require(isinstance(operation_id, str) and IDENTIFIER.fullmatch(operation_id) is not None, f"{where}: operationId {operation_id!r} is not a TypeScript identifier")
            require(operation_id not in seen, f"{where}: operationId {operation_id} is used twice")
            seen.add(operation_id)
            extra = set(operation) - {
                "operationId",
                "tags",
                "summary",
                "description",
                "parameters",
                "requestBody",
                "responses",
                "security",
                "deprecated",
            }
            require(not extra, f"{where}: keyword(s) {sorted(extra)} are not rendered")
            description = "\n\n".join(
                text for text in (operation.get("summary"), operation.get("description")) if text
            )
            one = INDENT
            two = INDENT * 2
            three = INDENT * 3
            out.extend(jsdoc(description or f"{where}", one, operation.get("deprecated", False)))
            out.append(f"{one}{operation_id}: {{")
            out.append(f"{two}method: {json.dumps(method.upper())};")
            out.append(f"{two}path: {json.dumps(path)};")
            parameters = operation.get("parameters", [])
            for key, location in (("pathParams", "path"), ("query", "query"), ("headers", "header")):
                out.append(f"{two}{key}: {self.parameter_group(parameters, location, where, two)};")
            out.append(f"{two}requestBody: {self.request_body(operation.get('requestBody'), where, two)};")
            responses = operation.get("responses", {})
            require(responses, f"{where}: no responses")
            out.append(f"{two}responses: {{")
            for code in sorted(responses):
                require(re.fullmatch(r"[1-5]\d\d", code) is not None, f"{where}: response key {code!r} is not a status code")
                out.append(f"{three}{code}: {self.response_type(responses[code], f'{where} {code}', three)};")
            out.append(f"{two}}};")
            out.append(f"{one}}};")
        out.append("}")
        return out


def header(document: dict, digest: str) -> list[str]:
    info = document.get("info", {})
    title = info.get("title", "")
    version = info.get("version", "")
    require(bool(title) and bool(version), "info.title and info.version are required for the header")
    return [
        f"// Generated by {SCRIPT_PATH} from {DOCUMENT_PATH}.",
        f"// {title}, document version {version}.",
        f"// Source SHA-256: {digest}",
        "//",
        "// DO NOT EDIT. This module is the OpenAPI document rendered as TypeScript.",
        "// A shape that is wrong here is wrong in the control plane's handlers:",
        "// change them, run `UPDATE_OPENAPI=1 cargo test -p deis-control --test",
        f"// openapi_drift`, then `python3 {SCRIPT_PATH}` and commit both files.",
        f"// CI runs `python3 {SCRIPT_PATH} --check` and refuses a document",
        "// whose client was not regenerated.",
        "",
    ]


def render(document: dict, digest: str) -> str:
    emitter = Emitter(document)
    lines = header(document, digest)
    lines.extend(emitter.schema_declarations())
    lines.extend(emitter.components_declaration())
    lines.extend(emitter.operations_declaration())
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# The emitter's own check: a small document that touches every construct
# the real one uses, and the exact module it must render as. A rendering
# that changes shows up here as a diff, before it reaches the console's
# type checker.
# ---------------------------------------------------------------------------

SELF_CHECK_DOCUMENT = {
    "openapi": "3.1.0",
    "info": {"title": "Self Check API", "version": "1.2.3"},
    "paths": {
        "/api/v1/apps/{app_id}": {
            "get": {
                "operationId": "getApp",
                "summary": "One application.",
                "parameters": [
                    {"name": "app_id", "in": "path", "required": True, "schema": {"type": "string"}},
                    {
                        "name": "limit",
                        "in": "query",
                        "required": False,
                        "description": "Page size.",
                        "schema": {"type": "integer", "format": "int32"},
                    },
                    {"name": "If-Match", "in": "header", "required": True, "schema": {"type": "string"}},
                ],
                "responses": {
                    "200": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/App"}}}},
                    "404": {"$ref": "#/components/responses/NotFound"},
                },
            },
            "delete": {
                "operationId": "deleteApp",
                "parameters": [
                    {"name": "app_id", "in": "path", "required": True, "schema": {"type": "string"}},
                ],
                "requestBody": {
                    "required": True,
                    "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Reason"}}},
                },
                "responses": {"204": {"description": "Gone."}},
            },
        }
    },
    "components": {
        "responses": {
            "NotFound": {
                "description": "No such application.",
                "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Problem"}}},
            }
        },
        "schemas": {
            "App": {
                "description": "An application.\nTwo lines of description.",
                "type": "object",
                "required": ["id", "kind", "labels", "created_at"],
                "properties": {
                    "id": {"type": "string", "format": "uuid"},
                    "kind": {"$ref": "#/components/schemas/Kind", "description": "Which kind."},
                    "labels": {"type": "object", "additionalProperties": {"type": "string"}, "propertyNames": {"type": "string"}},
                    "created_at": {"type": "integer", "format": "int64", "minimum": 0},
                    "note": {"type": ["string", "null"]},
                    "ports": {"type": ["array", "null"], "items": {"type": "integer"}},
                    "parent": {"oneOf": [{"type": "null"}, {"$ref": "#/components/schemas/App"}]},
                    "extra": {},
                    "$schema": {"type": "string"},
                },
            },
            "Kind": {"type": "string", "enum": ["web", "worker"]},
            "Reason": {
                "allOf": [
                    {"$ref": "#/components/schemas/Problem"},
                    {"type": "object", "required": ["why"], "properties": {"why": {"type": "string"}}},
                ]
            },
            "Problem": {
                "type": "object",
                "required": ["message"],
                "properties": {
                    "message": {"type": "string", "description": "What went wrong; ends with */ here."},
                    "detail": {"type": "object"},
                },
                "additionalProperties": False,
            },
            "Tag": {"oneOf": [{"type": "string", "enum": ["a"]}, {"type": "object", "required": ["custom"], "properties": {"custom": {"type": "string"}}}]},
            "Either": {
                "allOf": [
                    {
                        "oneOf": [
                            {"type": "object", "required": ["x"], "properties": {"x": {"type": "string"}}},
                            {"type": "object", "required": ["y"], "properties": {"y": {"type": "string"}}},
                        ]
                    },
                    {"$ref": "#/components/schemas/Problem"},
                    {"type": "array", "items": {"oneOf": [{"type": "string"}, {"type": "number"}]}},
                ]
            },
        },
    },
}

SELF_CHECK_EXPECTED = """\
// Generated by scripts/generate-openapi-types.py from contracts/openapi/v1.json.
// Self Check API, document version 1.2.3.
// Source SHA-256: 0000
//
// DO NOT EDIT. This module is the OpenAPI document rendered as TypeScript.
// A shape that is wrong here is wrong in the control plane's handlers:
// change them, run `UPDATE_OPENAPI=1 cargo test -p deis-control --test
// openapi_drift`, then `python3 scripts/generate-openapi-types.py` and commit both files.
// CI runs `python3 scripts/generate-openapi-types.py --check` and refuses a document
// whose client was not regenerated.

/**
 * An application.
 * Two lines of description.
 */
export type App = {
  id: string /* uuid */;
  /** Which kind. */
  kind: Kind;
  labels: { [key: string]: string };
  created_at: number /* int64 */;
  note?: string | null;
  ports?: number[] | null;
  parent?: null | App;
  extra?: unknown;
  $schema?: string;
};

export type Kind = "web" | "worker";

export type Reason = Problem & {
  why: string;
};

export type Problem = {
  /** What went wrong; ends with *\\/ here. */
  message: string;
  detail?: { [key: string]: unknown };
};

export type Tag = "a" | {
  custom: string;
};

export type Either = ({
  x: string;
} | {
  y: string;
}) & Problem & Array<string | number>;

/**
 * The schemas by the name the document gives them, for callers that
 * spell a type as `components["schemas"]["Name"]`.
 */
export interface components {
  schemas: {
    App: App;
    Kind: Kind;
    Reason: Reason;
    Problem: Problem;
    Tag: Tag;
    Either: Either;
  };
}

/**
 * Every operation, by `operationId`. `responses` is keyed by status code, so
 * `operations["listRuntimeFleet"]["responses"][200]` is what a 200 carries;
 * `never` marks a parameter group or body the operation does not take.
 */
export interface operations {
  /** One application. */
  getApp: {
    method: "GET";
    path: "/api/v1/apps/{app_id}";
    pathParams: {
      app_id: string;
    };
    query: {
      /** Page size. */
      limit?: number /* int32 */;
    };
    headers: {
      "If-Match": string;
    };
    requestBody: never;
    responses: {
      200: App;
      404: Problem;
    };
  };
  /** DELETE /api/v1/apps/{app_id} */
  deleteApp: {
    method: "DELETE";
    path: "/api/v1/apps/{app_id}";
    pathParams: {
      app_id: string;
    };
    query: never;
    headers: never;
    requestBody: Reason;
    responses: {
      204: void;
    };
  };
}
"""

# Constructs the emitter must refuse rather than render as `unknown`, each
# with the phrase its refusal names.
SELF_CHECK_REFUSALS = [
    ({"anyOf": [{"type": "string"}]}, "anyOf"),
    ({"type": "object", "patternProperties": {"^x": {"type": "string"}}}, "patternProperties"),
    ({"type": ["string", "integer"]}, "one type plus null"),
    ({"type": "integer", "enum": [1, 2]}, "only rendered over strings"),
    ({"$ref": "#/components/schemas/Missing"}, "does not define"),
    ({"type": "array"}, "array without items"),
    ({"type": "object", "properties": {"a": {"type": "string"}}, "additionalProperties": {"type": "string"}}, "additionalProperties schema"),
    ({"not": {"type": "string"}}, "not"),
]


def self_check() -> None:
    rendered = render(SELF_CHECK_DOCUMENT, "0000")
    if rendered != SELF_CHECK_EXPECTED:
        diff = "".join(
            difflib.unified_diff(
                SELF_CHECK_EXPECTED.splitlines(keepends=True),
                rendered.splitlines(keepends=True),
                "expected",
                "rendered",
            )
        )
        raise GeneratorError(f"self-check: the inline document renders differently:\n{diff}")
    emitter = Emitter(SELF_CHECK_DOCUMENT)
    for schema, phrase in SELF_CHECK_REFUSALS:
        try:
            emitter.render_type(schema, "self-check", "")
        except GeneratorError as error:
            require(phrase in str(error), f"self-check: {schema} was refused for another reason: {error}")
        else:
            raise GeneratorError(f"self-check: {schema} was rendered instead of refused")


# ---------------------------------------------------------------------------
# Entry point.
# ---------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument(
        "--check",
        action="store_true",
        help=f"do not write; exit 1 with a diff when {OUTPUT_PATH} differs from what {DOCUMENT_PATH} renders",
    )
    args = parser.parse_args()

    try:
        self_check()
        document, digest = load_document(ROOT / DOCUMENT_PATH)
        rendered = render(document, digest)
    except (OSError, KeyError, TypeError, ValueError, GeneratorError) as error:
        print(f"openapi types generation failed: {error}", file=sys.stderr)
        return 1

    schemas = len(document.get("components", {}).get("schemas", {}))
    operations = sum(1 for _ in Emitter(document).operations())
    output = ROOT / OUTPUT_PATH
    summary = f"{schemas} schemas and {operations} operations from {DOCUMENT_PATH} ({document['info']['version']}, sha256 {digest[:12]})"

    if args.check:
        committed = output.read_text(encoding="utf-8") if output.exists() else ""
        if committed == rendered:
            print(f"openapi types check passed: {OUTPUT_PATH} is what {summary} renders")
            return 0
        diff = list(
            difflib.unified_diff(
                committed.splitlines(keepends=True),
                rendered.splitlines(keepends=True),
                f"committed {OUTPUT_PATH}",
                f"rendered from {DOCUMENT_PATH}",
                n=2,
            )
        )
        shown = diff[:60]
        print(
            f"openapi types check failed: {OUTPUT_PATH} is not what {DOCUMENT_PATH} renders "
            f"({'missing' if not committed else f'{len(diff)} diff lines, first {len(shown)} shown'}):",
            file=sys.stderr,
        )
        sys.stderr.write("".join(shown))
        if not "".join(shown).endswith("\n"):
            sys.stderr.write("\n")
        print(f"Run `python3 {SCRIPT_PATH}` and commit {OUTPUT_PATH}.", file=sys.stderr)
        return 1

    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists() and output.read_text(encoding="utf-8") == rendered:
        print(f"openapi types unchanged: {OUTPUT_PATH} already is what {summary} renders")
        return 0
    output.write_text(rendered, encoding="utf-8")
    print(f"openapi types written: {OUTPUT_PATH} from {summary}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
