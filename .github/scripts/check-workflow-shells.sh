#!/usr/bin/env bash
# Parse every `run:` block in `.github/workflows/*.yml` with `bash -n`, without running any of it.
#
# Why this file exists: on 2026-11-13 the release workflow failed at its third step of four — after a full llama.cpp build —
# because a heredoc line in `Write the cmake toolchain file` had its quotes nested one character wrong:
#
#   prefix=$(dirname "$(command -v x86_64-w64-mingw32-gcc")")/x86_64-w64-mingw32-
#
# That is not a YAML error, so the file loads; it is not a Rust error, so no gate sees it; and Actions does not parse a step's
# script until the step starts. The tree paid for a Windows cross-build to discover a shell-syntax typo — the most expensive
# way anyone has found a typo in this repository. `bash -n` catches it in milliseconds:
#
#   bash: line 12: unexpected EOF while looking for matching `"'
#
# What this does **not** claim: that the scripts are correct, or that they would work on a runner. It checks that each one is a
# program bash can read. `${{ … }}` is replaced with a word first, because Actions substitutes those before a shell ever sees
# them and an unsubstituted `${{` is itself a bash syntax error — the substitution models the runner rather than grading it.
#
# Extraction is indentation-based YAML reading (`run: |` opens a block that ends at the next line indented no deeper; `run: cmd`
# is one line) and needs no Python or PyYAML: this row has to run on a box with nothing but bash, since that is what "checkable
# locally" means here. Blocks are handed over as files rather than through a pipe because a step's script is multi-line by
# definition and any line-oriented transport would grade one bash program per line — which is a bug this script's first draft
# had, and which showed up as 237 "run blocks" in four workflows.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
fail=0
checked=0

for workflow in .github/workflows/*.yml; do
  awk -v out="$scratch" -v file="$workflow" '
    BEGIN { count = 0 }
    function flush(   n) {
      if (!inblock) return
      n = sprintf("%s/step-%03d.sh", out, ++count)
      printf "%s", body > n
      close(n)
      printf "%s\t%s\t%s\n", file, label, n
      inblock = 0; body = ""
    }
    # `run: |`, `run: |-`, `run: >` … — the script starts on the following line.
    /^[[:space:]]*run:[[:space:]]*[|>]/ {
      flush()
      block_indent = match($0, /[^[:space:]]/)
      block_line = NR
      label = "run block at line " NR
      inblock = 1
      next
    }
    # `run: some/command --with args` — one line, no block.
    /^[[:space:]]*run:[[:space:]]*[^|>[:space:]]/ {
      flush()
      line = $0
      sub(/^[[:space:]]*run:[[:space:]]*/, "", line)
      n = sprintf("%s/step-%03d.sh", out, ++count)
      printf "%s\n", line > n
      close(n)
      printf "%s\t%s\t%s\n", file, "run at line " NR, n
      next
    }
    inblock {
      # A block scalar ends at the first non-empty line indented no deeper than the `run:` key itself.
      if ($0 ~ /^[[:space:]]*$/ || match($0, /[^[:space:]]/) > block_indent) { body = body $0 "\n"; next }
      flush()
    }
    END { flush() }
  ' "$workflow" > "$scratch/index"

  while IFS=$'\t' read -r file label step; do
    [ -f "${step:-}" ] || continue
    checked=$((checked + 1))
    # Actions substitutes expressions before the shell sees them; `${{ … }}` → `x` models that.
    sed 's/\${{[^}]*}}/x/g' "$step" > "$step.sub"
    if ! err=$(bash -n "$step.sub" 2>&1); then
      printf '%s\n' "$err" | sed "s|$step[.]sub|$file ($label)|" >&2
      echo "^ $file: the $label is not a program bash can read" >&2
      fail=1
    fi
  done < "$scratch/index"
done

if [ "$fail" -ne 0 ]; then
  echo "workflow shell check: FAILED ($checked run block(s) parsed)" >&2
  exit 1
fi
echo "workflow shell check: $checked run block(s) parse"
