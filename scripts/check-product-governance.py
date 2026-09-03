#!/usr/bin/env python3
"""Check Deislet's ADR, standard lock, conformance and roadmap invariants."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
EXPECTED_ADRS = list(range(1, 22))
EXPECTED_PHASES = [f"phase-{number}" for number in range(6)]
GIT_SHA = re.compile(r"^[0-9a-f]{40}$")
WORK_ID = re.compile(r"^P([0-5])-[A-Z]+-[0-9]{3}$")


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


def validate_adrs() -> None:
    adr_dir = ROOT / "docs/adr"
    files = sorted(adr_dir.glob("[0-9][0-9][0-9]-*.md"))
    numbers = [int(path.name[:3]) for path in files]
    require(numbers == EXPECTED_ADRS, f"ADR set must be exactly 001..021, got {numbers}")

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

    pinned_documents = [
        "docs/adr/README.md",
        "docs/product/README.md",
        "docs/product/roadmap.json",
        *[str(path.relative_to(ROOT)) for path in sorted((ROOT / "docs/adr").glob("[0-9][0-9][0-9]-*.md"))],
    ]
    for relative in pinned_documents:
        require(commit in read_text(relative), f"{relative}: does not pin the locked Edge Canon commit")
    return commit


def validate_roadmap() -> None:
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


def validate_integration() -> None:
    workflow = read_text(".github/workflows/checks.yml")
    require(
        "python3 scripts/check-product-governance.py" in workflow,
        "CI does not run the product governance check",
    )
    implementation = read_text("docs/implementation-plan.zh.md")
    require("文档边界（2026-09-03）" in implementation, "legacy implementation spec lacks its authority boundary")
    product = read_text("docs/product/README.md")
    require("没有后端可依据 `edge-canon.next` 宣称正式支持" in product, "current support boundary is ambiguous")


def main() -> int:
    try:
        validate_adrs()
        commit = validate_standard_lock()
        validate_roadmap()
        validate_integration()
    except (KeyError, TypeError, ValidationError) as error:
        print(f"product governance validation failed: {error}", file=sys.stderr)
        return 1

    print(
        "product governance validation passed: 21 ADRs, 6 roadmap phases, "
        f"Edge Canon {commit[:7]} locked, no temporary paths"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
