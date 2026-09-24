#!/usr/bin/env bash
# Fails when a relative Markdown link points at a file that does not exist.
#
# Every Markdown document in the repository is scanned, except anything under
# docs/archive/: that directory is frozen history and its internal links are
# no longer maintained. Links from maintained documents into docs/archive/
# are still checked, so an archived file that is renamed or deleted again
# breaks the build instead of leaving dangling references behind.
#
# Inline links and images of the form `[text](target)` are checked. Absolute
# URLs, mailto links, same-document anchors and reference-style definitions
# are not. Link fragments (`target.md#section`) are checked down to the file.
#
# Two trees are exempt:
#   - docs/archive/, frozen history whose internal links are no longer
#     maintained;
#   - examples/, vendored third-party reference projects whose documentation
#     is trimmed from upstream and not maintained here.
#
# Usage:
#   scripts/check-doc-links.sh              check the repository
#   scripts/check-doc-links.sh --self-test  run the negative cases first

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Prints one link target per line for the given document. Only the parenthesised
# form is scanned; this repository does not use angle-bracket targets.
doc_link_targets() {
  local document=$1
  grep -oE '\]\([^)]+\)' "$document" 2>/dev/null \
    | sed -e 's/^\](`*//' -e 's/`*)$//' -e 's/^\](//' -e 's/)$//' \
    | while IFS= read -r target; do
        case "$target" in
          http://* | https://* | mailto:* | '#'*) continue ;;
          '' ) continue ;;
        esac
        printf '%s\n' "$target"
      done
}

# Checks every Markdown document under $1 except $2 (the archive) and $3 (a
# single excluded tree). Prints one line per broken target and returns the
# number of failures.
check_tree() {
  local base=$1 archive=$2 excluded=$3 failures=0
  local document dir target path
  while IFS= read -r -d '' document; do
    [ "$document" != "$archive" ] || continue
    [ "$document" != "$excluded" ] || continue
    case "$document" in
      "$archive"/* | "$excluded"/*) continue ;;
    esac
    dir="$(dirname "$document")"
    while IFS= read -r target; do
      [ -n "$target" ] || continue
      path="${target%%#*}"
      [ -n "$path" ] || continue
      if [ ! -e "$dir/$path" ]; then
        printf '%s: broken link target: %s\n' "${document#"$base"/}" "$target" >&2
        failures=$((failures + 1))
      fi
    done < <(doc_link_targets "$document")
  done < <(find "$base" -name '*.md' \
    -not -path '*/.git/*' -not -path '*/target/*' -not -path '*/node_modules/*' \
    -print0 | sort -z)
  return "$failures"
}

self_test() {
  local work
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' RETURN
  mkdir -p "$work/docs/archive" "$work/vendor"

  # A well-formed tree: every link resolves, including one into the archive.
  printf '# A\n\nSee [b](b.md) and [history](archive/old.md).\n' > "$work/docs/a.md"
  printf '# B\n' > "$work/docs/b.md"
  printf '# Old\n' > "$work/docs/archive/old.md"

  if ! check_tree "$work" "$work/docs/archive" "$work/vendor" > /dev/null 2>&1; then
    echo "self-test failed: the well-formed tree was reported broken" >&2
    return 1
  fi

  # A broken link inside the excluded tree must not be reported.
  printf '# Vendored\n\nSee [gone](gone.md).\n' > "$work/vendor/v.md"

  # A broken link outside the archive must be reported...
  printf '# A\n\nSee [missing](missing.md).\n' > "$work/docs/a.md"
  local out status
  out="$(check_tree "$work" "$work/docs/archive" "$work/vendor" 2>&1)" && status=0 || status=$?
  if [ "$status" -eq 0 ] || [ "$(printf '%s\n' "$out" | grep -c 'missing.md')" -ne 1 ]; then
    echo "self-test failed: the broken link outside docs/archive was not reported" >&2
    return 1
  fi

  # ...but a broken link inside docs/archive must not be.
  printf '# Old\n\nSee [gone](gone.md).\n' > "$work/docs/archive/old.md"
  out="$(check_tree "$work" "$work/docs/archive" "$work/vendor" 2>&1)" && status=0 || status=$?
  if [ "$status" -eq 0 ] || [ "$(printf '%s\n' "$out" | grep -c 'missing.md')" -ne 1 ] \
    || [ "$(printf '%s\n' "$out" | grep -c 'gone.md')" -ne 0 ]; then
    echo "self-test failed: archive-internal links must be exempt from the check" >&2
    return 1
  fi

  echo "check-doc-links.sh self-test passed"
}

if [ "${1:-check}" = "--self-test" ]; then
  self_test
  exit 0
fi

failures=0
check_tree "$root" "$root/docs/archive" "$root/examples" || failures=$?
if [ "$failures" -gt 0 ]; then
  echo "check-doc-links: $failures broken link target(s)" >&2
  exit 1
fi
echo "check-doc-links: all relative Markdown link targets exist"
