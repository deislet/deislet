#!/usr/bin/env bash
#
# Fail when a document spells out an HTTP path that `deis_wire::conventions`
# already defines a constant for.
#
# Why this exists
# ---------------
# The node-facing paths have one definition, in `crates/deis-wire/src/
# conventions.rs`, because nothing in the compiler checks that two crates spell
# an HTTP path the same way: a node that asked for `/register` while the control
# plane served `/registration` compiled perfectly, and the only symptom was a
# fleet that never learned about its own nodes. The same hazard applies to a
# document, with one difference - a document is where a reader goes to find out
# what to call, so a document that spells the path itself becomes a second
# definition that nothing keeps in step. That mistake has now been made twice
# (`POST .../register` in DEMO.md, and again in the review before last), which
# is why it is a check and not a convention.
#
# What is refused
# ---------------
# In any Markdown file:
#
#   1. A literal path that a constant defines, on a line that does not name the
#      constant. Naming it is the fix - `deis_wire::conventions::
#      NODE_ASSIGNMENTS_ROUTE`, or `node_assignments_path()` for the client
#      side - with the literal, if it is worth showing at all, in a comment
#      beside the name.
#   2. A path in the same family whose last segment is a near miss of one the
#      constants define (`.../register` for `.../registration`). That is a
#      wrong path rather than an undocumented one, so it is refused everywhere,
#      including in the exempt files below.
#
# A path in the family that is nothing like a defined one - `/api/nodes/:id/
# heartbeats`, a route that has since moved - is left alone. This check knows
# only what `conventions.rs` says it owns.
#
# Everything it knows is read out of `conventions.rs` at run time: the paths,
# the constant names and the client-side helpers. Nothing here is a second copy
# of the thing it is protecting.
#
# Usage: check-doc-path-literals.sh [path ...]     (default: the whole repo)

set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CONVENTIONS="${ROOT_DIR}/crates/deis-wire/src/conventions.rs"

# Files allowed to spell a path out without naming the constant on the line.
#
# Each entry is a repository-relative path and needs a reason and a way out.
#
#   crates/deis-control/API_DOCUMENTATION.md
#     The control plane's own HTTP reference. Its readers hold a curl command
#     rather than a Rust crate, so the literal path is the content there, not a
#     restatement of it. Retire this entry by adding one sentence to the "Node
#     Endpoints" section naming NODE_REGISTRATION_ROUTE and
#     NODE_ASSIGNMENTS_ROUTE as where those paths come from, and then deleting
#     this line.
#
#   docs/implementation-plan.zh.md
#     A dated record of decisions already taken. Its section 4 does name the
#     constant; the security note further down restates the path in prose, and
#     rewriting the prose of a record after the fact is its own kind of wrong.
#     Retire this entry when that note is next revised.
EXEMPT_FILES=(
  "crates/deis-control/API_DOCUMENTATION.md"
  "docs/implementation-plan.zh.md"
)

# How many leading characters two segment names must share before one is read
# as a misspelling of the other rather than an unrelated route. `register` and
# `registration` share eight; `heartbeats` and `assignments` share none.
NEAR_MISS_PREFIX=5

if [ ! -f "$CONVENTIONS" ]; then
  echo "❌ Cannot find ${CONVENTIONS}; this check reads the constants from it" >&2
  exit 2
fi

# ----------------------------------------------------------------------
# What conventions.rs says it owns
# ----------------------------------------------------------------------
#
# One record per route constant: family|tail|constant|helper
#   family   the path up to and including the segment before the parameter
#   tail     the segment after the parameter
#   constant the name to reference instead of the literal
#   helper   the client-side function, when conventions.rs defines one
ROUTES=()

while IFS= read -r line; do
  name=${line%%:*}
  value=${line#*=}
  value=${value#*\"}
  value=${value%\"*}

  case "$value" in
    */:*) ;;
    *) continue ;;  # not a parameterised route template
  esac

  family=${value%%:*}          # "/api/nodes/"
  rest=${value#*:}             # "id/registration"
  tail=${rest#*/}              # "registration"
  case "$tail" in
    */*) continue ;;           # more than one segment after the parameter
  esac

  helper=$(echo "$name" | tr 'A-Z' 'a-z')
  helper=${helper%_route}_path
  if ! grep -q "pub fn ${helper}" "$CONVENTIONS"; then
    helper=""
  fi

  ROUTES+=("${family}|${tail}|${name}|${helper}")
done < <(grep -E '^pub const [A-Z0-9_]+: &str = "/[^"]*";' "$CONVENTIONS" \
  | sed -E 's/^pub const ([A-Z0-9_]+): &str = (.*);$/\1:=\2/')

if [ ${#ROUTES[@]} -eq 0 ]; then
  echo "❌ No route constants found in ${CONVENTIONS}." >&2
  echo "   Either they moved, or their spelling changed and this check has" >&2
  echo "   stopped protecting anything. It fails rather than passing empty." >&2
  exit 2
fi

# Every symbol a document may name instead of a literal, for the "is the
# constant on this line?" test.
SYMBOLS=()
for route in "${ROUTES[@]}"; do
  IFS='|' read -r _family _tail constant helper <<<"$route"
  SYMBOLS+=("$constant")
  [ -n "$helper" ] && SYMBOLS+=("$helper")
done

# ----------------------------------------------------------------------
# The documents
# ----------------------------------------------------------------------

is_exempt() {
  local relative=$1
  local exempt
  for exempt in "${EXEMPT_FILES[@]}"; do
    [ "$relative" = "$exempt" ] && return 0
  done
  return 1
}

names_a_symbol() {
  local text=$1
  local symbol
  for symbol in "${SYMBOLS[@]}"; do
    case "$text" in
      *"$symbol"*) return 0 ;;
    esac
  done
  return 1
}

# Length of the common leading run of two strings.
common_prefix_length() {
  local a=$1 b=$2 i=0
  while [ "$i" -lt "${#a}" ] && [ "$i" -lt "${#b}" ]; do
    [ "${a:$i:1}" != "${b:$i:1}" ] && break
    i=$((i + 1))
  done
  echo "$i"
}

targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
  targets=("$ROOT_DIR")
fi

documents=$(find "${targets[@]}" \
  \( -path '*/target' -o -path '*/node_modules' -o -path '*/examples' \
     -o -path '*/.git' -o -path '*/.demo' -o -path '*/dist' \) -prune \
  -o -type f -name '*.md' -print | sort)

failures=0
scanned=0

while IFS= read -r file; do
  [ -z "$file" ] && continue
  scanned=$((scanned + 1))
  relative=${file#"${ROOT_DIR}/"}
  exempt=0
  is_exempt "$relative" && exempt=1

  for route in "${ROUTES[@]}"; do
    IFS='|' read -r family known_tail constant helper <<<"$route"
    reference="deis_wire::conventions::${constant}"
    [ -n "$helper" ] && reference="${reference} (client side: ${helper}())"

    # `:` is legal in a path segment written as a route template (`:id`), so
    # the segment pattern excludes only what really ends one.
    while IFS= read -r hit; do
      [ -z "$hit" ] && continue
      lineno=${hit%%:*}
      rest=${hit#*:}
      literal=${rest%%:*}
      text=${rest#*:}
      seen_tail=${literal##*/}

      if [ "$seen_tail" = "$known_tail" ]; then
        [ "$exempt" -eq 1 ] && continue
        names_a_symbol "$text" && continue
        echo "❌ ${relative}:${lineno}: writes the path \"${literal}\" out in full."
        echo "   That path is defined once, by ${reference}."
        echo "   Name the constant here instead; if the value itself is worth"
        echo "   showing, put it in a comment beside the name. A document that"
        echo "   spells it out is a second definition, and nothing keeps the two"
        echo "   in step."
        echo "   The line: ${text}"
        echo
        failures=$((failures + 1))
        continue
      fi

      shared=$(common_prefix_length "$seen_tail" "$known_tail")
      if [ "$shared" -ge "$NEAR_MISS_PREFIX" ]; then
        echo "❌ ${relative}:${lineno}: \"${literal}\" is not a path this platform serves."
        echo "   It is a near miss of ${reference}, which ends in \"${known_tail}\","
        echo "   not \"${seen_tail}\". A node that asked for the wrong one compiled"
        echo "   perfectly and simply never joined the fleet."
        echo "   The line: ${text}"
        echo
        failures=$((failures + 1))
      fi
    done < <(grep -noE "${family}[^/[:space:]\`\"']+/[A-Za-z][A-Za-z0-9_-]*" "$file" \
      | while IFS= read -r found; do
          n=${found%%:*}
          p=${found#*:}
          printf '%s:%s:%s\n' "$n" "$p" "$(sed -n "${n}p" "$file" | tr -s '[:space:]' ' ')"
        done)
  done
done <<<"$documents"

if [ "$failures" -ne 0 ]; then
  echo "${failures} document path literal(s) refused across ${scanned} document(s)."
  exit 1
fi

echo "✓ ${scanned} document(s) checked; every node-facing path is referenced by its constant."
