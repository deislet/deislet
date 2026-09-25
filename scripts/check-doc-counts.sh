#!/usr/bin/env bash
#
# Fail when a maintained document writes down how many checks or tests there
# are, and when the demo's own count and its registry disagree.
#
# Why this exists
# ---------------
# The number of checks `demo.sh verify` runs is defined once, by the
# `group_<name>_checks` arrays under `scripts/demo/checks/`; `demo.sh` sums
# them into `VERIFY_EXPECTED_CHECKS` and holds each run to it. A second copy
# of that number in prose is a second definition, and nothing keeps the two in
# step. It has already drifted: on one day the registry listed 101 checks
# while README.md said 94 and DEMO.md said 95, and a reader had no way to tell
# which of the three the run they were looking at had printed.
#
# What is refused
# ---------------
# In README.md, DEMO.md and every Markdown file under docs/ except
# `docs/archive/`, a numeral followed by a counter and a test or check noun:
#
#   1. `N 项检查`, `N 条测试`, `N 个用例` and the like;
#   2. `N checks` / `N tests`, the English form.
#
# A numeral is a decimal number, a CJK numeral, or an English number word
# (`ninety-four checks`). `一` is left out of the CJK set on purpose: `一项
# 检查` reads as "a check", not as a count. So is the singular `test`, for the
# same reason.
#
# The fix is never to write a different number. Name the group - the arrays
# are the registry, and a group has a stable name (`streaming responses`,
# `group_streaming_checks`) - or say what is true of the checks without
# counting them. What a reader needs the number for, `verify` prints: its last
# line is the tally and the roll call, and `VERIFY_EXPECTED_CHECKS` is the
# number it is held to.
#
# What else is checked
# --------------------
# `VERIFY_EXPECTED_CHECKS` must equal the number of checks the registry
# actually registers: the sum of the `group_<name>_checks` arrays, for the
# groups `DEMO_CHECK_GROUPS` names. This is not `grep -c '^check_'`: a
# function can exist without being registered in any group, and two checks
# (`check_static_asset`, `check_secret_delivery`) print a second verdict, so
# the count of verdicts a run prints is not the count of checks the registry
# lists either.
#
# Usage:
#   scripts/check-doc-counts.sh                 check the repository
#   scripts/check-doc-counts.sh --self-test     run the counter-examples first
#   scripts/check-doc-counts.sh <doc>...        check these documents (the
#                                              registry is still checked)

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEMO_ENTRY="${ROOT_DIR}/scripts/demo.sh"
CHECKS_DIR="${ROOT_DIR}/scripts/demo/checks"

# Documents allowed to write a count anyway.
#
# Each entry is a repository-relative path and needs a reason and a way out.
# Nothing is exempt today: the reason the check exists is that every one of
# these documents had a number that had gone stale.
EXEMPT_FILES=()

# A numeral: decimal digits, a CJK numeral without 一, or an English number
# word (optionally hyphenated, as in `ninety-four`).
NUMERAL='([0-9]+|[二三四五六七八九十百千两]+|\b(one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve|thirteen|fourteen|fifteen|sixteen|seventeen|eighteen|nineteen|twenty|thirty|forty|fifty|sixty|seventy|eighty|ninety|hundred)([ -](one|two|three|four|five|six|seven|eight|nine))?\b)'

# A count of checks or tests. `checks`/`tests` because a singular `test` reads
# as "a test" far more often than as a number; the CJK branch requires both a
# counter and the noun, so `十五项检查` is refused while `三项` alone is not.
COUNT_PATTERN="${NUMERAL}[[:space:]]*((项|条|个)[[:space:]]*(测试|检查|用例|断言)|\b(checks|tests)\b)"

# ----------------------------------------------------------------------
# The registry, and the number derived from it
# ----------------------------------------------------------------------

# The groups `DEMO_CHECK_GROUPS` names, one per line. Comments and the array
# delimiters are dropped; the registry is a plain list of words.
registry_groups() {
  local entry=$1
  awk '/^DEMO_CHECK_GROUPS=\(/ {inside = 1; next}
       inside && /^\)/ {exit}
       inside {print}' "$entry" \
    | sed -e 's/#.*//' \
    | tr -s '[:space:]' '\n' \
    | sed '/^$/d'
}

# The body of `group_<name>_checks=(...)`, one entry per line.
group_array_body() {
  local dir=$1 group=$2 file
  for file in "$dir"/*.sh; do
    [ -f "$file" ] || continue
    grep -q "^group_${group}_checks=" "$file" || continue
    awk -v name="group_${group}_checks=" '
      index($0, name) == 1 {
        rest = substr($0, length(name) + 1)
        if (rest ~ /^\(/) {
          sub(/^\(/, "", rest)
          if (index(rest, ")") > 0) {
            print substr(rest, 1, index(rest, ")") - 1)
            exit
          }
          inside = 1
          next
        }
      }
      inside && /^\)/ {exit}
      inside {print}
    ' "$file"
    return 0
  done
  return 1
}

# The checks registered for one group, printed as a number. Fails when the
# group has no array: a group the registry names and nothing registers is a
# run that would print fewer verdicts than it is held to.
registered_group_checks() {
  local dir=$1 group=$2 body
  body=$(group_array_body "$dir" "$group") || {
    echo "❌ scripts/demo/checks/ defines no group_${group}_checks array, and" >&2
    echo "   DEMO_CHECK_GROUPS names ${group}." >&2
    return 1
  }
  printf '%s\n' "$body" \
    | sed -e 's/#.*//' \
    | tr -s '[:space:]' '\n' \
    | sed '/^$/d' \
    | wc -l
}

# The checks the registry registers, printed as a number.
registered_checks() {
  local entry=$1 dir=$2 group total=0 count
  while IFS= read -r group; do
    [ -n "$group" ] || continue
    count=$(registered_group_checks "$dir" "$group") || return 1
    total=$((total + count))
  done < <(registry_groups "$entry")
  printf '%s' "$total"
}

# `VERIFY_EXPECTED_CHECKS` as the demo itself computes it. Sourcing the entry
# point defines the demo and returns; the number is read the way a run reads
# it, from the same code, so a change to the derivation cannot pass unnoticed
# here.
derived_checks() {
  local entry=$1
  bash -c 'set -euo pipefail
           source "$1" >/dev/null 2>&1
           printf "%s" "$VERIFY_EXPECTED_CHECKS"' _ "$entry"
}

# Fails when the derived number and the registry disagree.
check_consistency() {
  local entry=$1 dir=$2 derived registered
  derived=$(derived_checks "$entry") || {
    echo "❌ could not read VERIFY_EXPECTED_CHECKS from ${entry#"$ROOT_DIR"/}" >&2
    return 1
  }
  registered=$(registered_checks "$entry" "$dir") || return 1
  if [ "$derived" != "$registered" ]; then
    echo "❌ VERIFY_EXPECTED_CHECKS says ${derived}; the group arrays register ${registered}." >&2
    echo "   The number is derived in ${entry#"$ROOT_DIR"/} by summing the" >&2
    echo "   group_<name>_checks arrays the registry names. The two are computed" >&2
    echo "   from the same arrays, so a difference means one of them was written" >&2
    echo "   down instead: a check whose line was deleted from an array is a" >&2
    echo "   question that stopped being asked, and no count of what did run says" >&2
    echo "   anything about it." >&2
    return 1
  fi
  printf '%s' "$derived"
}

# ----------------------------------------------------------------------
# The documents
# ----------------------------------------------------------------------

is_exempt() {
  local relative=$1 exempt
  for exempt in "${EXEMPT_FILES[@]:-}"; do
    [ -n "$exempt" ] || continue
    [ "$relative" = "$exempt" ] && return 0
  done
  return 1
}

# Every document this check reads, printed one path per line.
default_documents() {
  printf '%s\n' "${ROOT_DIR}/README.md" "${ROOT_DIR}/DEMO.md"
  find "${ROOT_DIR}/docs" -type f -name '*.md' \
    -not -path "${ROOT_DIR}/docs/archive/*" -print | sort
}

# Prints one report per line that writes a count down, and returns the number
# of documents that did. Exits 2 for a path that is not there: a typo in an
# argument must not read as "nothing to check".
scan_documents() {
  local failures=0 file relative line lineno text
  for file in "$@"; do
    if [ ! -f "$file" ]; then
      echo "❌ no such document: ${file}" >&2
      exit 2
    fi
    relative=${file#"$ROOT_DIR"/}
    is_exempt "$relative" && continue
    while IFS= read -r hit; do
      [ -z "$hit" ] && continue
      lineno=${hit%%:*}
      text=${hit#*:}
      echo "❌ ${relative}:${lineno}: writes down a count of checks or tests."
      echo "   ${text}"
      failures=$((failures + 1))
    done < <(grep -nEi "$COUNT_PATTERN" "$file" || true)
  done
  return "$failures"
}

# ----------------------------------------------------------------------
# Counter-examples
# ----------------------------------------------------------------------

self_test() {
  local work
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN

  # A stand-in demo: two groups, five checks, and a registry that sums them
  # exactly the way scripts/demo.sh does.
  mkdir -p "$work/demo/checks"
  cat > "$work/demo/lib.sh" <<'FIXTURE'
demo_count_checks() {
  local group total=0
  for group in "$@"; do
    local -n group_checks="group_${group}_checks"
    total=$((total + ${#group_checks[@]}))
  done
  printf '%s' "$total"
}
FIXTURE
  cat > "$work/demo/checks/alpha.sh" <<'FIXTURE'
group_alpha_label="alpha"
group_alpha_checks=(
  check_one
  # a comment, which is not an entry
  check_two
)
FIXTURE
  cat > "$work/demo/checks/beta.sh" <<'FIXTURE'
group_beta_label="beta"
group_beta_checks=(
  check_three
  check_four
  check_five
)
FIXTURE
  # A third group whose file defines a label and no array: the registry names
  # three groups, so the derivation counts what is there and the registry
  # check has to report the gap rather than quietly agree with it.
  cat > "$work/demo/checks/gamma.sh" <<'FIXTURE'
group_gamma_label="gamma"
FIXTURE

  write_entry() {  # $1 dest, $2 groups as words, $3 how expected is computed
    local dest=$1 groups=$2 derivation=$3 group
    {
      printf 'DEMO_CHECK_GROUPS=(\n'
      for group in $groups; do printf '  %s\n' "$group"; done
      printf ')\nsource "%s/demo/lib.sh"\n' "$work"
      for group in $groups; do printf 'source "%s/demo/checks/%s.sh"\n' "$work" "$group"; done
      printf 'VERIFY_EXPECTED_CHECKS=%s\n' "$derivation"
    } > "$dest"
  }

  write_entry "$work/demo.sh" "alpha beta" '$(demo_count_checks "${DEMO_CHECK_GROUPS[@]}")'
  local reported
  if ! reported=$(check_consistency "$work/demo.sh" "$work/demo/checks"); then
    echo "self-test failed: a registry that sums its arrays was reported inconsistent" >&2
    return 1
  fi
  if [ "$reported" != 5 ]; then
    echo "self-test failed: expected 5 registered checks, got ${reported}" >&2
    return 1
  fi

  # A number written down instead of derived must be caught.
  write_entry "$work/demo-written.sh" "alpha beta" 6
  if check_consistency "$work/demo-written.sh" "$work/demo/checks" > /dev/null 2>&1; then
    echo "self-test failed: a hand-written VERIFY_EXPECTED_CHECKS was accepted" >&2
    return 1
  fi

  # A group the registry names and nothing registers must be caught.
  write_entry "$work/demo-missing.sh" "alpha beta gamma" '$(demo_count_checks "${DEMO_CHECK_GROUPS[@]}")'
  if check_consistency "$work/demo-missing.sh" "$work/demo/checks" > /dev/null 2>&1; then
    echo "self-test failed: a group with no array was accepted" >&2
    return 1
  fi

  # A document that counts its checks must be reported, in both forms...
  local status=0
  printf '# A\n\nThe demo runs 95 checks in all.\n' > "$work/counts.md"
  scan_documents "$work/counts.md" > /dev/null 2>&1 || status=$?
  if [ "$status" -eq 0 ]; then
    echo "self-test failed: a hand-written \"95 checks\" was accepted" >&2
    return 1
  fi
  printf '# A\n\n演示分二十一组跑九十五项检查，每项都打印证据。\n' > "$work/counts.md"
  status=0
  scan_documents "$work/counts.md" > /dev/null 2>&1 || status=$?
  if [ "$status" -eq 0 ]; then
    echo "self-test failed: a hand-written CJK count was accepted" >&2
    return 1
  fi

  # ...and a document that describes the checks without counting them must not.
  printf '# A\n\nThe streaming group drives each shape through deis-proxy.\n' > "$work/clean.md"
  if ! scan_documents "$work/clean.md" > /dev/null 2>&1; then
    echo "self-test failed: a document with no count was reported" >&2
    return 1
  fi

  echo "check-doc-counts.sh self-test passed"
}

if [ "${1:-check}" = "--self-test" ]; then
  self_test
  exit 0
fi

failures=0
check_consistency "$DEMO_ENTRY" "$CHECKS_DIR" > /dev/null || failures=$((failures + 1))

documents=("$@")
if [ ${#documents[@]} -eq 0 ]; then
  mapfile -t documents < <(default_documents)
fi
scanned=${#documents[@]}
counted=0
scan_documents "${documents[@]}" && counted=0 || counted=$?
failures=$((failures + counted))

if [ "$failures" -ne 0 ]; then
  cat <<'EOF'

   The count a run is held to is derived, not written: `VERIFY_EXPECTED_CHECKS`
   in `scripts/demo.sh`, summed from the `group_<name>_checks` arrays under
   `scripts/demo/checks/`. A number in prose is a second definition of it, and
   the two drift. Name the group instead, or say what is true of the checks
   without counting them.
EOF
  exit 1
fi

echo "✓ ${scanned} document(s) checked; no count of checks or tests is written down."
