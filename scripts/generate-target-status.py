#!/usr/bin/env python3
"""Render Deislet target status from the exact Edge Canon registry snapshot."""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
LOCK_PATH = ROOT / "standards/edge-canon.lock.json"
JSON_PATH = ROOT / "docs/product/target-status.json"
MARKDOWN_PATH = ROOT / "docs/product/target-status.zh.md"
CLI_JSON_PATH = ROOT / "crates/deis/src/commands/target-status.json"
WEB_JSON_PATH = ROOT / "crates/deis-control/web/src/generated/target-status.json"
MATURITY = {
    "artifact-generated": 0,
    "syntax-verified": 1,
    "deployed": 2,
    "conformance-passed": 3,
}
# Deislet-side target freeze policy (remediation D-5 / T5-1): frozen targets
# ship stateless function artifacts only. They accept no bindings and carry no
# certification promises; the registry snapshot stays upstream-owned, so the
# freeze is applied here as an overlay rather than by editing the snapshot.
FROZEN_BACKENDS = frozenset({"deno", "tencent-edgeone-makers"})
LIFECYCLE_STATES = ("active", "frozen")


class StatusError(RuntimeError):
    """A lock, registry or generated-view invariant did not hold."""


def load_object(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise StatusError(f"{path.relative_to(ROOT)}: {error}") from error
    if not isinstance(value, dict):
        raise StatusError(f"{path.relative_to(ROOT)}: top-level value must be an object")
    return value


def require(condition: bool, message: str) -> None:
    if not condition:
        raise StatusError(message)


def source_state() -> tuple[dict[str, Any], dict[str, Any], str]:
    lock = load_object(LOCK_PATH)
    source = lock.get("source")
    require(isinstance(source, dict), "standard lock source must be an object")
    snapshot_relative = source.get("registrySnapshotPath")
    require(isinstance(snapshot_relative, str), "standard lock lacks registrySnapshotPath")
    snapshot = ROOT / snapshot_relative
    try:
        snapshot_bytes = snapshot.read_bytes()
    except OSError as error:
        raise StatusError(f"{snapshot_relative}: {error}") from error
    actual_digest = hashlib.sha256(snapshot_bytes).hexdigest()
    require(
        actual_digest == source.get("registrySha256"),
        f"{snapshot_relative}: sha256 {actual_digest} does not match the standard lock",
    )
    registry = load_object(snapshot)

    standard = lock.get("standard")
    require(isinstance(standard, dict), "standard lock standard must be an object")
    require(registry.get("schemaVersion") == 1, "registry schemaVersion must be 1")
    require(
        registry.get("standardId") == standard.get("contractId"),
        "registry standardId does not match the locked contract",
    )
    require(
        registry.get("standardReleaseStatus") == standard.get("releaseStatus"),
        "registry release status does not match the standard lock",
    )
    require(
        registry.get("standardContract") == source.get("contractPath"),
        "registry points at a different standard contract",
    )

    policy = registry.get("certificationPolicy")
    require(isinstance(policy, dict), "registry certificationPolicy must be an object")
    backends = registry.get("backends")
    require(isinstance(backends, list) and backends, "registry backends must be a non-empty array")
    ids: set[str] = set()
    for backend in backends:
        require(isinstance(backend, dict), "every registry backend must be an object")
        backend_id = backend.get("id")
        require(isinstance(backend_id, str) and backend_id, "backend id must be a string")
        require(backend_id not in ids, f"duplicate registry backend {backend_id}")
        ids.add(backend_id)
        evidence = backend.get("evidence")
        certification = backend.get("certification")
        require(isinstance(evidence, dict), f"{backend_id}: evidence must be an object")
        require(isinstance(certification, dict), f"{backend_id}: certification must be an object")
        maturity = evidence.get("maturity")
        require(maturity in MATURITY, f"{backend_id}: unknown evidence maturity {maturity!r}")
        compliant = certification.get("compliant")
        supported = certification.get("supported")
        require(isinstance(compliant, bool), f"{backend_id}: compliant must be boolean")
        require(isinstance(supported, bool), f"{backend_id}: supported must be boolean")
        require(not supported or compliant, f"{backend_id}: supported without compliance")
        require(
            not compliant or maturity == "conformance-passed",
            f"{backend_id}: compliance lacks conformance-passed evidence",
        )
        if standard.get("normativeRelease") is False:
            require(
                not compliant and not supported,
                f"{backend_id}: a proposal standard cannot be compliant or supported",
            )
    return lock, registry, actual_digest


def check_lifecycle(registry: dict[str, Any]) -> None:
    """The freeze overlay must name real backends and promise nothing extra."""
    registered = {backend["id"] for backend in registry["backends"]}
    unknown = FROZEN_BACKENDS - registered
    require(
        not unknown,
        f"frozen backends missing from the registry: {sorted(unknown)}",
    )
    for backend in registry["backends"]:
        if backend["id"] in FROZEN_BACKENDS:
            require(
                not backend["certification"]["supported"],
                f"{backend['id']}: a frozen target cannot be supported",
            )
            require(
                not backend["certification"]["compliant"],
                f"{backend['id']}: a frozen target cannot be compliant",
            )


def target_status() -> dict[str, Any]:
    lock, registry, digest = source_state()
    check_lifecycle(registry)
    source = lock["source"]
    standard = lock["standard"]
    policy = registry["certificationPolicy"]
    return {
        "$schema": "target-status.schema.json",
        "schemaVersion": 1,
        "generatedFrom": {
            "repository": source["repository"],
            "sourceCommit": source["sourceCommit"],
            "registryPath": source["registryPath"],
            "registrySnapshotPath": source["registrySnapshotPath"],
            "registrySha256": digest,
            "registryUpdatedAt": registry["updatedAt"],
        },
        "standard": {
            "id": registry["standardId"],
            "version": standard["standardVersion"],
            "releaseStatus": registry["standardReleaseStatus"],
            "normativeRelease": standard["normativeRelease"],
            "certificationUnit": policy["certificationUnit"],
            "requiredMaturity": policy["requiredMaturity"],
        },
        "backends": [
            {
                "id": backend["id"],
                "displayName": backend["displayName"],
                # A frozen target is not first-class whatever the upstream
                # registry calls it: its role follows the freeze.
                "role": (
                    "experimental"
                    if backend["id"] in FROZEN_BACKENDS
                    else backend["role"]
                ),
                "lifecycle": (
                    "frozen" if backend["id"] in FROZEN_BACKENDS else "active"
                ),
                "implementation": backend["implementation"],
                "evidence": backend["evidence"],
                "certification": backend["certification"],
            }
            for backend in registry["backends"]
        ],
    }


def render_json(status: dict[str, Any]) -> str:
    return json.dumps(status, ensure_ascii=False, indent=2) + "\n"


def yes_no(value: bool) -> str:
    return "是" if value else "否"


def render_markdown(status: dict[str, Any]) -> str:
    source = status["generatedFrom"]
    standard = status["standard"]
    repository = source["repository"].removesuffix("/")
    registry_url = (
        f"{repository}/blob/{source['sourceCommit']}/{source['registryPath']}"
    )
    lines = [
        "<!-- Generated by scripts/generate-target-status.py; do not edit. -->",
        "# 目标后端证据状态",
        "",
        f"本页由 Edge Canon commit `{source['sourceCommit']}` 的",
        f"[conformance registry]({registry_url}) 确定性生成；快照 SHA-256 为",
        f"`{source['registrySha256']}`。它表达证据与认证状态，不表达供应商产品功能表。",
        "",
        f"标准 `{standard['id']}` 当前为 `{standard['releaseStatus']}`，",
        f"normative release：{yes_no(standard['normativeRelease'])}。",
        "只有完整标准版本达到 `conformance-passed` 且仍在维护窗口内，才可显示为支持。",
        "生命周期为 `frozen` 的目标仅生成无状态函数产物：不接受绑定，不承诺认证。",
        "",
        "| 后端 | 角色 | 生命周期 | 证据成熟度 | 证据结果 | 认证 | 合规 | 支持 | 有效期 |",
        "|---|---|---|---|---|---:|---:|---:|---|",
    ]
    for backend in status["backends"]:
        evidence = backend["evidence"]
        certification = backend["certification"]
        valid_until = certification["validUntil"] or "—"
        lines.append(
            "| {display} (`{id}`) | `{role}` | `{lifecycle}` | `{maturity}` | `{outcome}` | "
            "`{certification}` | {compliant} | {supported} | {valid_until} |".format(
                display=backend["displayName"],
                id=backend["id"],
                role=backend["role"],
                lifecycle=backend["lifecycle"],
                maturity=evidence["maturity"],
                outcome=evidence["outcome"],
                certification=certification["status"],
                compliant=yes_no(certification["compliant"]),
                supported=yes_no(certification["supported"]),
                valid_until=valid_until,
            )
        )
    lines.extend(["", "## 当前阻断项", ""])
    for backend in status["backends"]:
        lines.extend([f"### {backend['displayName']}", ""])
        blockers = backend["certification"]["blockers"]
        if blockers:
            lines.extend(f"- {blocker}" for blocker in blockers)
        else:
            lines.append("- 无")
        lines.append("")
    return "\n".join(lines)


def compare(path: Path, expected: str) -> bool:
    try:
        actual = path.read_text(encoding="utf-8")
    except OSError as error:
        print(f"{path.relative_to(ROOT)}: {error}", file=sys.stderr)
        return False
    if actual == expected:
        return True
    diff = difflib.unified_diff(
        actual.splitlines(),
        expected.splitlines(),
        fromfile=str(path.relative_to(ROOT)),
        tofile=f"generated:{path.relative_to(ROOT)}",
        lineterm="",
    )
    print("\n".join(diff), file=sys.stderr)
    return False


def main() -> int:
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true", help="fail if generated views drift")
    mode.add_argument("--write", action="store_true", help="rewrite generated views")
    args = parser.parse_args()
    try:
        status = target_status()
        outputs = {
            JSON_PATH: render_json(status),
            MARKDOWN_PATH: render_markdown(status),
            CLI_JSON_PATH: render_json(status),
            WEB_JSON_PATH: render_json(status),
        }
        if args.write:
            for path, content in outputs.items():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")
            print("generated target status from the locked Edge Canon registry")
            return 0
        comparisons = [compare(path, content) for path, content in outputs.items()]
        if all(comparisons):
            print("target status matches the locked Edge Canon registry")
            return 0
        return 1
    except StatusError as error:
        print(f"target status generation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
