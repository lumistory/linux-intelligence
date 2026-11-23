#!/bin/sh
# 0-F step 8: can a binary built on Arch run here?
# Three artefacts:
#   hello_dyn    - Rust, dynamically linked against Arch glibc (2.44)
#   hello_static - Rust, fully static
#   hello_newer  - C, deliberately requires glibc >= 2.43
set -u

echo "--- host container libc"
ldd --version 2>&1 | head -n 1

echo "--- artefact properties as seen from inside"
for f in hello_dyn hello_static hello_newer; do
    # Only tools guaranteed to exist in a minimal image are used here:
    # no `file`, no `strings` (absent on Debian/Ubuntu images).
    size="$(wc -c <"/probe-bin/$f" 2>/dev/null || echo '?')"
    magic="$(head -c 4 "/probe-bin/$f" 2>/dev/null | tr -d '\000-\037' || true)"
    printf '%-14s size=%-9s magic=%s\n' "$f" "$size" "${magic:-binary}"
done

echo "--- run 1/3: Rust dynamic (Arch glibc 2.44 build)"
/probe-bin/hello_dyn
printf 'hello_dyn exit = %s\n' "$?"

echo "--- run 2/3: Rust static"
/probe-bin/hello_static
printf 'hello_static exit = %s\n' "$?"

echo "--- run 3/3: C requiring GLIBC_2.43"
/probe-bin/hello_newer
printf 'hello_newer exit = %s\n' "$?"

echo "--- if a run failed, the loader error above is the evidence"
