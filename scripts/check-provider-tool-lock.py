#!/usr/bin/env python3
"""Validate provider tool source pins; optionally re-fetch exact distributions."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import sys
import urllib.request
from pathlib import Path

import jsonschema


ROOT = Path(__file__).resolve().parents[1]
LOCK_PATH = ROOT / "provider-drivers/official-tool-lock.json"
SCHEMA_PATH = ROOT / "provider-drivers/official-tool-lock.schema.json"
EXPECTED_PROVIDERS = ["cloudflare-workers-pages", "tencent-edgeone-makers"]
MAX_DISTRIBUTION_BYTES = 256 * 1024 * 1024


def load(path: Path) -> dict:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path.relative_to(ROOT)}: top-level JSON must be an object")
    return value


def validate_semantics(lock: dict) -> None:
    tools = lock["tools"]
    providers = [tool["provider"] for tool in tools]
    if providers != EXPECTED_PROVIDERS:
        raise ValueError(
            "official tool lock must contain Cloudflare then EdgeOne exactly once"
        )
    for tool in tools:
        expected_tarball = (
            f"{tool['registry']}/{tool['package']}/-/"
            f"{tool['package']}-{tool['version']}.tgz"
        )
        if tool["tarball"] != expected_tarball:
            raise ValueError(f"{tool['provider']}: tarball is not derived from its exact pin")
        automation = tool["serverAutomation"]
        if automation["status"] == "candidate":
            if automation["credentialInArgv"] or not automation["credentialTransport"]:
                raise ValueError(
                    f"{tool['provider']}: candidate needs a safe non-argv credential transport"
                )
        elif not automation["credentialInArgv"]:
            raise ValueError(
                f"{tool['provider']}: blocked record must preserve its observed argv conflict"
            )


def verify_download(tool: dict) -> None:
    request = urllib.request.Request(
        tool["tarball"], headers={"User-Agent": "deislet-provider-tool-lock/1"}
    )
    sha256 = hashlib.sha256()
    sha512 = hashlib.sha512()
    with urllib.request.urlopen(request, timeout=60) as response:  # noqa: S310
        if response.geturl() != tool["tarball"]:
            raise ValueError(f"{tool['provider']}: registry redirected the locked tarball")
        content_length = response.headers.get("Content-Length")
        if content_length is not None and int(content_length) > MAX_DISTRIBUTION_BYTES:
            raise ValueError(f"{tool['provider']}: tarball exceeds the download bound")
        total = 0
        while chunk := response.read(1024 * 1024):
            total += len(chunk)
            if total > MAX_DISTRIBUTION_BYTES:
                raise ValueError(f"{tool['provider']}: tarball exceeded the download bound")
            sha256.update(chunk)
            sha512.update(chunk)
    actual_sha256 = sha256.hexdigest()
    actual_integrity = "sha512-" + base64.b64encode(sha512.digest()).decode("ascii")
    if actual_sha256 != tool["distributionSha256"]:
        raise ValueError(
            f"{tool['provider']}: SHA-256 mismatch, observed {actual_sha256}"
        )
    if actual_integrity != tool["npmIntegrity"]:
        raise ValueError(
            f"{tool['provider']}: npm SRI mismatch, observed {actual_integrity}"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--verify-downloads",
        action="store_true",
        help="download the exact locked npm tarballs and recompute both digests",
    )
    args = parser.parse_args()
    try:
        schema = load(SCHEMA_PATH)
        lock = load(LOCK_PATH)
        jsonschema.Draft202012Validator.check_schema(schema)
        jsonschema.Draft202012Validator(
            schema, format_checker=jsonschema.FormatChecker()
        ).validate(lock)
        validate_semantics(lock)
        if args.verify_downloads:
            for tool in lock["tools"]:
                verify_download(tool)
    except (OSError, ValueError, json.JSONDecodeError, jsonschema.ValidationError) as error:
        print(f"provider tool lock validation failed: {error}", file=sys.stderr)
        return 1
    suffix = " and distributions" if args.verify_downloads else ""
    print(f"provider tool lock{suffix} validated: {len(lock['tools'])} exact pins")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
