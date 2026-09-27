#!/usr/bin/env bash
#
# Restore drill for the control plane: prove, on a machine that is not the
# production one, that the Litestream replica plus the key material plus the
# configuration are enough to bring a control plane up.
#
# Why this exists
# ---------------
# "The backup ran" is not evidence that anything can be restored, and ADR-012
# says so in as many words: only the most recent rehearsal is strong evidence.
# A Litestream replica that has been quietly failing, a master key nobody
# copied, a bundle directory on a disk that is not backed up - each of those
# looks exactly like a working backup until the day it is needed.
#
# What it does, all of it inside one isolated directory it creates:
#
#   1. `litestream restore` from the replica into <work>/control.db
#   2. `PRAGMA integrity_check`
#   3. `PRAGMA user_version`, and against --expect-user-version when given
#      (the restored database must not be newer than the binary serving it)
#   4. `deis-control --check-config` on a configuration generated for the
#      drill
#   5. row counts of the tables the installation's worth lives in
#   6. start `deis-control` against the restored database and call /health,
#      plus a read-only catalogue endpoint when --read-token is given
#   7. write a JSON report into docs/product/evidence/
#
# The control plane it starts binds loopback only and its configuration names
# neither a runtime node of the production fleet, nor a store/queue/telemetry
# endpoint, nor a provider driver installation root: nothing it does can reach
# or change anything outside the isolated directory.
#
# It never writes to the production database, and it never starts a second
# Litestream: restoring into a directory of its own is a read of the replica.
# It does read the production configuration, for the machine channel's
# certificate paths and the master key file, and copies those into the
# directory - a restore that has the database and not the key material is
# precisely the failure this is meant to catch.
#
# Usage: scripts/control-restore-drill.sh --replica <url> [options]

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PINNED_LITESTREAM_SERIES="0.5"
DEFAULT_CONTROL_CONFIG=/etc/deis/control.toml

REPLICA=""
CONTROL_CONFIG="${DEIS_DRILL_CONFIG:-}"
CONTROL_BIN=""
READ_TOKEN="${DEIS_RESTORE_DRILL_TOKEN:-}"
WORK_DIR=""
EVIDENCE_DIR="${REPO_ROOT}/docs/product/evidence"
EXPECT_USER_VERSION=""
SOURCE_DATABASE=""
MAX_SOURCE_LAG_SECONDS=300
API_LIMIT=100
KEEP=0
READY_TIMEOUT=60

usage() {
  cat <<'EOF'
Usage: control-restore-drill.sh --replica <url> [options]

  --replica URL            Litestream replica to restore from: s3://bucket/prefix,
                           file:///mnt/backup/control, ... (required)
  --config PATH            the installed control configuration. Default:
                           /etc/deis/control.toml, or $DEIS_DRILL_CONFIG
  --control-bin PATH       the control binary. Default: /usr/local/bin/deis-control,
                           then deis-control on PATH
  --work-dir DIR           where the isolated copy is made. Default: a new
                           directory under $TMPDIR
  --evidence-dir DIR       where the JSON report is written. Default:
                           <repository>/docs/product/evidence
  --read-token TOKEN       a credential with the read scope (or
                           $DEIS_RESTORE_DRILL_TOKEN). Without it the drill
                           still runs and reports the read-only call as not
                           made, and its outcome is "partial"
  --expect-user-version N  fail when the restored database's user_version is
                           above N. Use the version the binary serving it knows
  --source-database PATH   the installation the replica was taken from, read
                           read-only so the drill can say how far behind the
                           replica is. Default: <data_dir>/control.db from
                           --config, when that file exists
  --max-source-lag-seconds N
                           how far behind the source the restored data may be
                           (default 300, against an RPO of one second)
  --api-limit N            page size for the read-only call (default 100)
  --ready-timeout SECONDS  how long to wait for the restarted control plane to
                           answer (default 60)
  --keep                   keep the isolated directory, whatever the outcome
                           (it is kept on failure either way)
  -h, --help               this text

Exit status: 0 the drill passed or is partial, 1 an assertion failed, 2 usage,
3 a tool this drill needs is missing or unusable.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

require_tool() {
  local tool=$1 why=$2
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'error: %s is not installed (or not on PATH) and this drill needs it: %s\n' \
      "$tool" "$why" >&2
    exit 3
  }
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --replica) REPLICA=${2:?--replica needs a URL}; shift 2 ;;
    --config) CONTROL_CONFIG=${2:?--config needs a path}; shift 2 ;;
    --control-bin) CONTROL_BIN=${2:?--control-bin needs a path}; shift 2 ;;
    --work-dir) WORK_DIR=${2:?--work-dir needs a path}; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR=${2:?--evidence-dir needs a path}; shift 2 ;;
    --read-token) READ_TOKEN=${2:?--read-token needs a value}; shift 2 ;;
    --expect-user-version) EXPECT_USER_VERSION=${2:?--expect-user-version needs a number}; shift 2 ;;
    --source-database) SOURCE_DATABASE=${2:?--source-database needs a path}; shift 2 ;;
    --max-source-lag-seconds) MAX_SOURCE_LAG_SECONDS=${2:?--max-source-lag-seconds needs a number}; shift 2 ;;
    --api-limit) API_LIMIT=${2:?--api-limit needs a number}; shift 2 ;;
    --ready-timeout) READY_TIMEOUT=${2:?--ready-timeout needs a number}; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'error: unknown argument %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n $REPLICA ]] || die "--replica is required: this drill restores from a replica and has nothing to do without one"
[[ $EXPECT_USER_VERSION =~ ^[0-9]+$ || -z $EXPECT_USER_VERSION ]] \
  || die "--expect-user-version takes a whole number, got ${EXPECT_USER_VERSION}"
[[ $API_LIMIT =~ ^[1-9][0-9]*$ ]] || die "--api-limit takes a positive whole number"
[[ $READY_TIMEOUT =~ ^[1-9][0-9]*$ ]] || die "--ready-timeout takes a positive whole number"
[[ $MAX_SOURCE_LAG_SECONDS =~ ^[0-9]+$ ]] || die "--max-source-lag-seconds takes a whole number"

# A source that was named and cannot be read is a refusal rather than a note:
# the run would otherwise report itself as evidence without the one check it
# was asked for. A source derived from the configuration is simply skipped.
if [[ -n $SOURCE_DATABASE ]]; then
  [[ -r $SOURCE_DATABASE ]] || die "--source-database $SOURCE_DATABASE is not readable"
fi

# The tools. `litestream` missing is the one to say clearly rather than fail on
# two steps later, because "this host has no Litestream" and "the replica is
# unreadable" are different problems with different fixes.
require_tool litestream "the replica is Litestream's own format"
require_tool python3 "reading the restored database and writing the report"
require_tool curl "calling the restored control plane"

if [[ -z $CONTROL_BIN ]]; then
  if [[ -x /usr/local/bin/deis-control ]]; then
    CONTROL_BIN=/usr/local/bin/deis-control
  else
    CONTROL_BIN=$(command -v deis-control) || {
      printf 'error: no control binary found: neither /usr/local/bin/deis-control nor deis-control on PATH. Pass --control-bin\n' >&2
      exit 3
    }
  fi
fi
[[ -x $CONTROL_BIN ]] || {
  printf 'error: %s is not executable\n' "$CONTROL_BIN" >&2
  exit 3
}

if [[ -z $CONTROL_CONFIG ]]; then
  CONTROL_CONFIG=$DEFAULT_CONTROL_CONFIG
fi
[[ -r $CONTROL_CONFIG ]] || {
  printf 'error: no readable control configuration at %s. Pass --config; the drill copies the machine channel identity and the master key from it, and cannot tell whether a restore is complete without it\n' \
    "$CONTROL_CONFIG" >&2
  exit 3
}

# The pinned series, checked rather than assumed: the configuration sample in
# this repository and the flags below are written for it, and the replica
# formats are not interchangeable between series.
litestream_version=$(litestream version 2>&1 | head -1)
if [[ ! $litestream_version =~ ${PINNED_LITESTREAM_SERIES}\. ]]; then
  printf 'error: litestream reports "%s", and this drill is written for the pinned %s series (0.5.17): the configuration sample, the replica format and the -json flag all differ between series\n' \
    "$litestream_version" "$PINNED_LITESTREAM_SERIES" >&2
  exit 3
fi

if [[ -z $WORK_DIR ]]; then
  WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/control-restore-drill.XXXXXX")
fi
WORK_DIR=$(mkdir -p "$WORK_DIR" && cd "$WORK_DIR" && pwd)
if [[ -e "$WORK_DIR/control.db" ]]; then
  die "--work-dir $WORK_DIR already holds a database: a drill restores into a directory it creates, so that nothing it writes can be mistaken for the installation's own data"
fi

DRILL_PY="$WORK_DIR/drill.py"
write_helper() {
  cat >"$DRILL_PY" <<'PY'
"""Helper for scripts/control-restore-drill.sh: configuration, inspection and
the JSON report. Nothing here decides whether the drill passed; it records what
happened, and `report` computes the assertions from that."""

import argparse
import datetime
import json
import os
import shutil
import socket
import sqlite3
import stat
import sys
import tomllib

# The tables an installation's worth lives in: what it deploys, where it is
# deployed, who may call it, and what happened. A restore that has an empty
# table where one of these should be is a restore that lost data quietly.
TABLES = [
    "apps",
    "versions",
    "environments",
    "deployments",
    "routes",
    "secrets",
    "secret_revisions",
    "accounts",
    "api_tokens",
    "nodes",
    "events",
    "audit_log",
]


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def stamp():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def toml_string(value):
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def write_json(path, value):
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(value, handle, indent=2, sort_keys=False)
        handle.write("\n")


def copy_material(source, target, mode):
    shutil.copyfile(source, target)
    os.chmod(target, mode)
    return {"source": source, "target": target}


def cmd_prepare(args):
    with open(args.source_config, "rb") as handle:
        source = tomllib.load(handle)

    machine = source.get("machine")
    if not machine:
        raise SystemExit(
            "error: %s has no [machine] section, and a control plane without one "
            "refuses to start at all - so there is no restored installation to "
            "drill. Point --config at the configuration the installation runs."
            % args.source_config
        )

    work = args.work_dir
    for directory in ("bundles", "keys", "machine"):
        os.makedirs(os.path.join(work, directory), exist_ok=True)
    copied = []
    key_target = None
    if source.get("secret_key_file"):
        key_target = os.path.join(work, "keys", "master.key")
        copied.append(
            copy_material(source["secret_key_file"], key_target, stat.S_IRUSR | stat.S_IWUSR)
        )

    identity = {}
    target_of = {}
    for field in ("certificate", "private_key", "trust_root"):
        if not machine.get(field):
            raise SystemExit("error: [machine] names no %s" % field)
        source_path = machine[field]
        if source_path in target_of:
            identity[field] = target_of[source_path]
            continue
        name = os.path.basename(source_path)
        target = os.path.join(work, "machine", name)
        if os.path.exists(target) or target in target_of.values():
            target = os.path.join(work, "machine", "%s-%s" % (field, name))
        mode = stat.S_IRUSR | stat.S_IWUSR if field == "private_key" else stat.S_IRUSR | stat.S_IRGRP | stat.S_IROTH
        copied.append(copy_material(source_path, target, mode))
        target_of[source_path] = target
        identity[field] = target

    control_port = free_port()
    machine_port = free_port()
    while machine_port == control_port:
        machine_port = free_port()

    config = os.path.join(work, "control.toml")
    lines = [
        "# Written by scripts/control-restore-drill.sh. This is not the",
        "# installation's configuration: it binds loopback, it names no runtime",
        "# node of the production fleet, no storage endpoint and no provider",
        "# driver installation root, so the control plane started from it can",
        "# reach and change nothing outside this directory. What it does copy is",
        "# the identity and the key material a restore needs.",
        "bind_addr = \"127.0.0.1\"",
        "port = %d" % control_port,
        "heartbeat_interval = %d" % int(source.get("heartbeat_interval", 30)),
        "heartbeat_timeout = %d" % int(source.get("heartbeat_timeout", 90)),
        "data_dir = %s" % toml_string(work),
        "bundle_dir = %s" % toml_string(os.path.join(work, "bundles")),
    ]
    if key_target:
        lines.append("secret_key_file = %s" % toml_string(key_target))
    lines += [
        "",
        "[machine]",
        "bind_addr = \"127.0.0.1\"",
        "port = %d" % machine_port,
        "certificate = %s" % toml_string(identity["certificate"]),
        "private_key = %s" % toml_string(identity["private_key"]),
        "trust_root = %s" % toml_string(identity["trust_root"]),
        "",
    ]
    with open(config, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines))

    write_json(
        os.path.join(work, "prepare.json"),
        {
            "configuration": config,
            "database": os.path.join(work, "control.db"),
            "dataDir": work,
            "bundleDir": os.path.join(work, "bundles"),
            "controlPort": control_port,
            "machinePort": machine_port,
            "masterKeyFile": key_target,
            "machineIdentity": identity,
            "copied": copied,
            "sourceConfiguration": args.source_config,
            "sourceBundleDir": source.get("bundle_dir"),
            "sourceDatabase": os.path.join(str(source.get("data_dir", "")), "control.db"),
        },
    )
    print("drill configuration written to %s (loopback only, no production node)" % config)


def open_inspection(path, allow_write):
    """A connection that writes nothing.

    Read-only, always, when the file is not ours to touch: a live production
    database must not be opened read-write by a drill, because opening one can
    checkpoint its WAL. A restored copy can carry a `-wal` beside it, and
    SQLite cannot read a WAL database read-only without being able to map its
    `-shm`, which is the one case where the caller allows a writable
    connection - to a file in a directory whose whole point is that nothing
    outside it can be reached.
    """
    if not os.path.exists(path + "-wal") or not allow_write:
        return sqlite3.connect("file:%s?mode=ro" % path, uri=True), True
    return sqlite3.connect(path), False


def row_counts(connection):
    counts = {}
    for table in TABLES:
        try:
            counts[table] = int(connection.execute("SELECT COUNT(*) FROM %s" % table).fetchone()[0])
        except sqlite3.Error:
            counts[table] = None
    return counts


def data_timestamp(connection):
    """The newest record in a database: its event log or its audit log, which
    is the point in time its contents represent."""
    newest = []
    try:
        row = connection.execute(
            "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', MAX(at), 'unixepoch') FROM audit_log"
        ).fetchone()
        if row and row[0]:
            newest.append(str(row[0]))
    except sqlite3.Error:
        pass
    try:
        row = connection.execute(
            "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', MAX(datetime(timestamp))) FROM events"
        ).fetchone()
        if row and row[0]:
            newest.append(str(row[0]))
    except sqlite3.Error:
        pass
    return max(newest) if newest else None


def lag_seconds(source_timestamp, restored_timestamp):
    if not source_timestamp or not restored_timestamp:
        return None
    parse = lambda value: datetime.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
        tzinfo=datetime.timezone.utc
    )
    return int((parse(source_timestamp) - parse(restored_timestamp)).total_seconds())


def cmd_inspect(args):
    connection, read_only = open_inspection(args.database, allow_write=True)
    try:
        integrity = [str(row[0]) for row in connection.execute("PRAGMA integrity_check")]
        user_version = int(connection.execute("PRAGMA user_version").fetchone()[0])
        counts = row_counts(connection)
        app_ids = [str(row[0]) for row in connection.execute("SELECT id FROM apps ORDER BY id")]
        restored_timestamp = data_timestamp(connection)
    finally:
        connection.close()

    # What the replica is compared against: the installation it was taken
    # from, read read-only, when it is still there to be read. A rehearsal on
    # a live installation is where "is the replica actually current?" can be
    # answered - during a real recovery the source is gone.
    comparison = None
    failure = None
    if args.source_database:
        if not os.path.exists(args.source_database):
            failure = "the source database %s does not exist" % args.source_database
        else:
            try:
                source, _ = open_inspection(args.source_database, allow_write=False)
                try:
                    source_counts = row_counts(source)
                    source_timestamp = data_timestamp(source)
                finally:
                    source.close()
                comparison = {
                    "path": args.source_database,
                    "tableDeltas": {
                        table: {
                            "source": source_counts.get(table),
                            "restored": counts.get(table),
                            "delta": (
                                source_counts[table] - counts[table]
                                if source_counts.get(table) is not None and counts.get(table) is not None
                                else None
                            ),
                        }
                        for table in TABLES
                    },
                    "sourceDataTimestamp": source_timestamp,
                    "restoredDataTimestamp": restored_timestamp,
                    "lagSeconds": lag_seconds(source_timestamp, restored_timestamp),
                    "maxLagSeconds": args.max_lag_seconds,
                }
            except sqlite3.Error as error:
                failure = "the source database %s could not be read: %s" % (
                    args.source_database,
                    error,
                )

    bundle_dir = args.bundle_dir or None
    zip_files = None
    if bundle_dir and os.path.isdir(bundle_dir):
        zip_files = sum(1 for _, _, files in os.walk(bundle_dir) for name in files if name.endswith(".zip"))

    write_json(
        args.json,
        {
            "integrityCheck": integrity,
            "userVersion": user_version,
            "rowCounts": counts,
            "appIds": app_ids,
            "readOnlyConnection": read_only,
            "dataTimestamp": restored_timestamp,
            "databaseBytes": os.path.getsize(args.database),
            "sourceComparison": comparison,
            "sourceComparisonFailure": failure,
            "bundleStore": {
                "path": bundle_dir,
                "exists": bool(bundle_dir) and os.path.isdir(bundle_dir),
                "zipFiles": zip_files,
            },
        },
    )
    print("integrity_check: %s; user_version: %d" % (", ".join(integrity), user_version))
    if comparison:
        print(
            "newest record: source %s, restored %s (lag %ss)"
            % (
                comparison["sourceDataTimestamp"],
                comparison["restoredDataTimestamp"],
                comparison["lagSeconds"],
            )
        )
    if failure:
        print("note: %s" % failure)


def cmd_api_check(args):
    """Compare the read-only page the restored control plane served with what
    the database actually holds: the point of starting it at all is that the
    process reads *this* database, and a page that does not match it would be
    the one failure a status code cannot show."""
    with open(args.page, "r", encoding="utf-8") as handle:
        page = json.load(handle)

    connection, _ = open_inspection(args.database, allow_write=False)
    try:
        app_ids = [str(row[0]) for row in connection.execute("SELECT id FROM apps ORDER BY id")]
    finally:
        connection.close()

    items = page.get("items", [])
    returned = [str(item.get("id")) for item in items]
    expected = app_ids[: args.limit]
    write_json(
        args.json,
        {
            "requested": True,
            "limit": args.limit,
            "items": len(items),
            "reportedTotal": page.get("page", {}).get("total"),
            "matchesDatabase": returned == expected,
            "expected": expected,
            "returned": returned,
        },
    )


def env(name, default=None):
    value = os.environ.get(name)
    return default if value in (None, "") else value


def env_int(name):
    value = env(name)
    return int(value) if value is not None else None


def load_json(path):
    if not os.path.exists(path):
        return None
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def cmd_report(args):
    work = args.work_dir
    prepare = load_json(os.path.join(work, "prepare.json")) or {}
    inspect = load_json(os.path.join(work, "inspect.json"))
    api = load_json(os.path.join(work, "api.json"))

    restore_status = env_int("RESTORE_EXIT_STATUS")
    restore_path = env("RESTORE_PATH", prepare.get("database"))
    restore_started = env("RESTORE_STARTED_AT")
    restore_finished = env("RESTORE_FINISHED_AT")
    restored_bytes = None
    if restore_path and os.path.exists(restore_path):
        restored_bytes = os.path.getsize(restore_path)

    check_config_status = env_int("CHECK_CONFIG_EXIT_STATUS")
    check_config_output = [
        line for line in (env("CHECK_CONFIG_OUTPUT", "").splitlines()) if line.strip()
    ]

    health_status = env_int("HEALTH_STATUS")
    health_body = env("HEALTH_BODY")
    api_status = env_int("API_STATUS")

    assertions = []

    def assert_that(identifier, description, passed, detail):
        assertions.append(
            {
                "id": identifier,
                "description": description,
                "passed": bool(passed),
                "detail": detail,
            }
        )

    assert_that(
        "replica-restored",
        "litestream restore wrote a non-empty database into the isolated directory",
        restore_status == 0 and bool(restored_bytes),
        "exit status %s, %s bytes at %s" % (restore_status, restored_bytes, restore_path),
    )
    assert_that(
        "integrity-check",
        "PRAGMA integrity_check answered ok",
        bool(inspect) and inspect.get("integrityCheck") == ["ok"],
        str((inspect or {}).get("integrityCheck")),
    )

    expected_user_version = env_int("EXPECT_USER_VERSION")
    if expected_user_version is not None and inspect:
        assert_that(
            "user-version-known",
            "the restored schema version is not above the version this binary knows",
            inspect["userVersion"] <= expected_user_version,
            "restored %s, binary knows up to %s" % (inspect["userVersion"], expected_user_version),
        )

    comparison = (inspect or {}).get("sourceComparison")
    if comparison and comparison.get("lagSeconds") is not None:
        assert_that(
            "replica-is-current",
            "the restored data is no further behind the installation it came from "
            "than the RPO allows",
            comparison["lagSeconds"] <= comparison["maxLagSeconds"],
            "newest record: source %s, restored %s, lag %ss (allowed %ss)"
            % (
                comparison["sourceDataTimestamp"],
                comparison["restoredDataTimestamp"],
                comparison["lagSeconds"],
                comparison["maxLagSeconds"],
            ),
        )

    assert_that(
        "configuration-usable",
        "deis-control --check-config accepted the configuration the restored installation runs",
        check_config_status == 0
        and any("configuration is usable" in line for line in check_config_output),
        "exit status %s" % check_config_status,
    )
    assert_that(
        "control-answers",
        "the control plane started against the restored database answered /health",
        health_status == 200 and health_body == "OK",
        "status %s, body %r" % (health_status, health_body),
    )
    if api is not None and api.get("requested"):
        assert_that(
            "read-only-api-matches-database",
            "the application list served from the restored database matches its own table",
            api_status == 200 and api.get("matchesDatabase") is True,
            "status %s, %s item(s), matches=%s" % (api_status, api.get("items"), api.get("matchesDatabase")),
        )

    failed = [entry for entry in assertions if not entry["passed"]]
    if failed:
        outcome = "failed"
    elif api is None or not api.get("requested"):
        outcome = "partial"
    else:
        outcome = "passed"

    limitations = []
    if api is None or not api.get("requested"):
        limitations.append(
            "No read credential was given, so the drill called /health only. "
            "/health is a constant: it shows the listener is up, not that the "
            "restored catalogue can be read. Pass --read-token for the full drill."
        )
    if inspect:
        missing = [table for table, count in inspect["rowCounts"].items() if count is None]
        if missing:
            limitations.append("Tables not present in the restored database: %s" % ", ".join(missing))
        limitations.append(
            "The bundle store is counted, not verified: bundle contents are "
            "content-addressed and restored by the file-sync job of ADR-012, "
            "which checks each zip's sha256."
        )
    if inspect is not None and not comparison:
        limitations.append(
            "The replica was not compared with the installation it came from, so "
            "this run says the replica can be restored and says nothing about how "
            "current it is: %s"
            % ((inspect or {}).get("sourceComparisonFailure") or "no source database was named")
        )
    if comparison and comparison.get("lagSeconds") is None:
        limitations.append(
            "Neither database recorded a timestamp to compare (no event and no "
            "audit line yet), so the replica's lag was not checked."
        )
    if prepare.get("sourceBundleDir") and not inspect:
        limitations.append("The restored database could not be inspected.")
    limitations.append(
        "The source database path is derived from data_dir; an installation whose "
        "unit passes --database explicitly may keep it under another name."
    )

    report = {
        "$schema": "https://github.com/deislet/deislet/docs/product/evidence/control-restore-drill.schema.json",
        "schemaVersion": 1,
        "evidenceId": "control-restore-drill-%s" % stamp(),
        "collectedAt": utc_now(),
        "outcome": outcome,
        "scope": (
            "control plane restore: a Litestream replica restored into an isolated "
            "directory, integrity-checked, and served by a control plane started "
            "against the restored database"
        ),
        "source": {
            "database": env("SOURCE_DATABASE", prepare.get("sourceDatabase")),
            "configuration": prepare.get("sourceConfiguration"),
            "replica": env("REPLICA_URL"),
            "litestream": env("LITESTREAM_VERSION"),
            "control": env("CONTROL_BIN"),
            "controlVersion": env("CONTROL_VERSION"),
        },
        "restore": {
            "path": restore_path,
            "startedAt": restore_started,
            "finishedAt": restore_finished,
            "durationMs": env_int("RESTORE_DURATION_MS"),
            "exitStatus": restore_status,
            "reportedDurationMs": env_int("LITESTREAM_DURATION_MS"),
            "txid": env("LITESTREAM_TXID"),
        },
        "integrityCheck": {
            "mode": "full",
            "result": ", ".join((inspect or {}).get("integrityCheck", []) or ["not run"]),
        },
        "userVersion": {
            "value": (inspect or {}).get("userVersion"),
            "expected": expected_user_version,
            "checked": expected_user_version is not None,
        },
        "rowCounts": (inspect or {}).get("rowCounts", {}),
        "dataTimestamp": (inspect or {}).get("dataTimestamp"),
        "sourceComparison": comparison,
        "databaseBytes": restored_bytes,
        "bundleStore": (inspect or {}).get(
            "bundleStore",
            {"path": prepare.get("sourceBundleDir"), "exists": False, "zipFiles": None},
        ),
        "drillConfiguration": {
            "path": prepare.get("configuration"),
            "checkConfig": {
                "exitStatus": check_config_status,
                "output": check_config_output,
            },
        },
        "control": {
            "started": env("CONTROL_STARTED") == "yes",
            "port": env_int("CONTROL_PORT"),
            "health": {"status": health_status, "body": health_body},
            "stopped": env("CONTROL_STOPPED") == "yes",
            "logPath": env("CONTROL_LOG"),
        },
        "readOnlyApi": api
        if api is not None and api.get("requested")
        else {
            "requested": False,
            "reason": "no read credential was given (--read-token / DEIS_RESTORE_DRILL_TOKEN)",
            "status": None,
        },
        "assertions": assertions,
        "limitations": limitations,
    }
    if api is not None and api.get("requested"):
        report["readOnlyApi"]["status"] = api_status

    with open(args.json, "w", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")

    if args.schema and os.path.exists(args.schema):
        try:
            import jsonschema
        except ImportError:
            print(
                "warning: jsonschema is not installed, so the report was not checked "
                "against %s" % args.schema,
                file=sys.stderr,
            )
        else:
            with open(args.schema, "r", encoding="utf-8") as handle:
                schema = json.load(handle)
            jsonschema.validate(report, schema)
            print("report validated against %s" % os.path.basename(args.schema))

    print("outcome: %s" % outcome)
    for entry in assertions:
        print("  %s %s" % ("PASS" if entry["passed"] else "FAIL", entry["id"]))
    raise SystemExit(0 if outcome != "failed" else 1)


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)

    prepare = sub.add_parser("prepare")
    prepare.add_argument("--source-config", required=True)
    prepare.add_argument("--work-dir", required=True)
    prepare.set_defaults(run=cmd_prepare)

    inspect = sub.add_parser("inspect")
    inspect.add_argument("--database", required=True)
    inspect.add_argument("--bundle-dir", default=None)
    inspect.add_argument("--source-database", default=None)
    inspect.add_argument("--max-lag-seconds", type=int, default=300)
    inspect.add_argument("--json", required=True)
    inspect.set_defaults(run=cmd_inspect)

    api_check = sub.add_parser("api-check")
    api_check.add_argument("--database", required=True)
    api_check.add_argument("--page", required=True)
    api_check.add_argument("--limit", type=int, required=True)
    api_check.add_argument("--json", required=True)
    api_check.set_defaults(run=cmd_api_check)

    report = sub.add_parser("report")
    report.add_argument("--work-dir", required=True)
    report.add_argument("--json", required=True)
    report.add_argument("--schema", default=None)
    report.set_defaults(run=cmd_report)

    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
PY
  chmod 700 "$DRILL_PY"
}

write_helper

step() { printf '\n== %s ==\n' "$1"; }

# Milliseconds since the epoch. `date +%s%3N` is not what it looks like on every
# coreutils build (some emit every nanosecond digit), and the duration of the
# restore is one of the two numbers this whole drill exists to produce.
now_ms() { printf '%s' $(( $(date +%s%N) / 1000000 )); }

# --------------------------------------------------------------------------
# The configuration the restored installation will be started with, and the
# key material copied into the directory. This is what makes the drill a test
# of the whole restore set rather than of one file.
# --------------------------------------------------------------------------
step "prepare an isolated installation"
python3 "$DRILL_PY" prepare --source-config "$CONTROL_CONFIG" --work-dir "$WORK_DIR"
read_prepare() {
  python3 -c 'import json,sys; value=json.load(open(sys.argv[1])).get(sys.argv[2]); print(value if value is not None else "")' \
    "$WORK_DIR/prepare.json" "$1"
}
CONTROL_PORT=$(read_prepare controlPort)
SOURCE_BUNDLE_DIR=$(read_prepare sourceBundleDir)
if [[ -z $SOURCE_DATABASE ]]; then
  SOURCE_DATABASE=$(read_prepare sourceDatabase)
fi
if [[ ! -r $SOURCE_DATABASE ]]; then
  SOURCE_DATABASE=""
fi

# --------------------------------------------------------------------------
# 1. The replica, restored into the isolated directory.
# --------------------------------------------------------------------------
step "litestream restore"
RESTORE_STARTED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
restore_start_ms=$(now_ms)
set +e
litestream restore -json -o "$WORK_DIR/control.db" "$REPLICA" \
  >"$WORK_DIR/restore.json" 2>"$WORK_DIR/restore.stderr"
RESTORE_EXIT_STATUS=$?
set -e
restore_end_ms=$(now_ms)
RESTORE_FINISHED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
export RESTORE_EXIT_STATUS RESTORE_STARTED_AT RESTORE_FINISHED_AT
export RESTORE_DURATION_MS=$((restore_end_ms - restore_start_ms))
export RESTORE_PATH="$WORK_DIR/control.db"

LITESTREAM_DURATION_MS=""
LITESTREAM_TXID=""
if [[ -s "$WORK_DIR/restore.json" ]]; then
  LITESTREAM_DURATION_MS=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("duration_ms",""))' "$WORK_DIR/restore.json" 2>/dev/null || true)
  LITESTREAM_TXID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("txid",""))' "$WORK_DIR/restore.json" 2>/dev/null || true)
fi
export LITESTREAM_DURATION_MS LITESTREAM_TXID

export REPLICA_URL="$REPLICA"
export LITESTREAM_VERSION="$litestream_version"
export CONTROL_BIN="$CONTROL_BIN"
CONTROL_VERSION=$("$CONTROL_BIN" --version 2>/dev/null | head -1 || true)
export CONTROL_VERSION
export EXPECT_USER_VERSION="$EXPECT_USER_VERSION"

if [[ $RESTORE_EXIT_STATUS -ne 0 ]]; then
  printf 'litestream restore failed (exit %s):\n' "$RESTORE_EXIT_STATUS" >&2
  sed 's/^/  /' "$WORK_DIR/restore.stderr" >&2 || true
fi

# --------------------------------------------------------------------------
# 2 and 3. Integrity, schema version and row counts, all from a read-only
# connection to the restored file.
# --------------------------------------------------------------------------
step "inspect the restored database"
if [[ -f "$WORK_DIR/control.db" ]]; then
  python3 "$DRILL_PY" inspect \
    --database "$WORK_DIR/control.db" \
    --bundle-dir "${SOURCE_BUNDLE_DIR:-}" \
    --source-database "${SOURCE_DATABASE:-}" \
    --max-lag-seconds "$MAX_SOURCE_LAG_SECONDS" \
    --json "$WORK_DIR/inspect.json" || true
fi

# --------------------------------------------------------------------------
# 4. The startup gate, on the configuration this installation would run with.
# --------------------------------------------------------------------------
step "deis-control --check-config"
set +e
CHECK_CONFIG_OUTPUT=$("$CONTROL_BIN" --config "$WORK_DIR/control.toml" \
  --database "$WORK_DIR/control.db" --check-config 2>&1)
CHECK_CONFIG_EXIT_STATUS=$?
set -e
export CHECK_CONFIG_EXIT_STATUS CHECK_CONFIG_OUTPUT
printf '%s\n' "$CHECK_CONFIG_OUTPUT"

# --------------------------------------------------------------------------
# 5. Start it, against the restored database, on loopback.
# --------------------------------------------------------------------------
step "start a control plane on the restored database"
CONTROL_LOG="$WORK_DIR/control.log"
"$CONTROL_BIN" --config "$WORK_DIR/control.toml" --database "$WORK_DIR/control.db" \
  >"$CONTROL_LOG" 2>&1 &
CONTROL_PID=$!
export CONTROL_LOG CONTROL_PORT
CONTROL_STARTED=no
CONTROL_STOPPED=no
HEALTH_STATUS=""
HEALTH_BODY=""

deadline=$(( $(date +%s) + READY_TIMEOUT ))
while [[ $(date +%s) -lt $deadline ]]; do
  if ! kill -0 "$CONTROL_PID" 2>/dev/null; then
    break
  fi
  body=$(curl -sS -m 2 -o - "http://127.0.0.1:${CONTROL_PORT}/health" 2>/dev/null || true)
  if [[ -n $body ]]; then
    HEALTH_BODY=$body
    HEALTH_STATUS=$(curl -sS -m 2 -o /dev/null -w '%{http_code}' \
      "http://127.0.0.1:${CONTROL_PORT}/health" 2>/dev/null || true)
    CONTROL_STARTED=yes
    break
  fi
  sleep 1
done

if [[ $CONTROL_STARTED == no ]]; then
  printf 'the control plane did not answer on 127.0.0.1:%s within %ss; its log ends:\n' \
    "$CONTROL_PORT" "$READY_TIMEOUT" >&2
  tail -n 20 "$CONTROL_LOG" >&2 || true
fi

# --------------------------------------------------------------------------
# 6. The read-only call, when a credential was given: /health is a constant,
# and a catalogue endpoint read out of the restored file is the point.
# --------------------------------------------------------------------------
if [[ $CONTROL_STARTED == yes && -n $READ_TOKEN ]]; then
  step "read the restored catalogue"
  set +e
  API_STATUS=$(curl -sS -m 10 -o "$WORK_DIR/apps.json" -w '%{http_code}' \
    -H "Authorization: Bearer ${READ_TOKEN}" \
    "http://127.0.0.1:${CONTROL_PORT}/api/apps?limit=${API_LIMIT}")
  api_exit=$?
  set -e
  if [[ $api_exit -eq 0 && $API_STATUS == 200 ]]; then
    python3 "$DRILL_PY" api-check \
      --database "$WORK_DIR/control.db" \
      --page "$WORK_DIR/apps.json" \
      --limit "$API_LIMIT" \
      --json "$WORK_DIR/api.json"
  else
    printf 'the read-only call answered %s (curl exit %s); body: %s\n' \
      "$API_STATUS" "$api_exit" "$(head -c 400 "$WORK_DIR/apps.json" 2>/dev/null || true)" >&2
    echo '{"requested": true, "limit": '"$API_LIMIT"', "items": null, "matchesDatabase": false}' \
      >"$WORK_DIR/api.json"
  fi
  export API_STATUS
fi

# --------------------------------------------------------------------------
# Stop it, and wait for the process to be gone: a drill that leaves a control
# plane running against a copy of production data is worse than no drill.
# --------------------------------------------------------------------------
if kill -0 "$CONTROL_PID" 2>/dev/null; then
  step "stop the control plane"
  kill -TERM "$CONTROL_PID" 2>/dev/null || true
  stop_deadline=$(( $(date +%s) + 30 ))
  while kill -0 "$CONTROL_PID" 2>/dev/null && [[ $(date +%s) -lt $stop_deadline ]]; do
    sleep 1
  done
  if kill -0 "$CONTROL_PID" 2>/dev/null; then
    kill -KILL "$CONTROL_PID" 2>/dev/null || true
    wait "$CONTROL_PID" 2>/dev/null || true
  else
    CONTROL_STOPPED=yes
    wait "$CONTROL_PID" 2>/dev/null || true
  fi
fi
export CONTROL_STARTED HEALTH_STATUS HEALTH_BODY CONTROL_STOPPED

# --------------------------------------------------------------------------
# 7. The report.
# --------------------------------------------------------------------------
step "write the evidence"
mkdir -p "$EVIDENCE_DIR"
REPORT="$EVIDENCE_DIR/control-restore-drill-$(date -u +%Y%m%dT%H%M%SZ).json"
set +e
python3 "$DRILL_PY" report \
  --work-dir "$WORK_DIR" \
  --json "$REPORT" \
  --schema "${REPO_ROOT}/docs/product/evidence/control-restore-drill.schema.json"
report_status=$?
set -e

printf '\nreport: %s\nisolated directory: %s\n' "$REPORT" "$WORK_DIR"
if [[ $report_status -eq 0 && $KEEP -eq 0 ]]; then
  rm -rf "$WORK_DIR"
else
  printf 'the isolated directory is kept: it holds the restored database, the control log and the replica output\n'
fi
exit "$report_status"
