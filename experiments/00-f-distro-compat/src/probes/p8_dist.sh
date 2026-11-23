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
    printf '%s: ' "$f"
    file "/probe-bin/$f" 2>&1 | sed 's/^[^:]*: //'
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
