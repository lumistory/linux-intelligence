#!/bin/sh
# 0-F step 5: libc identity and ABI ceiling.
set -u

echo "--- ldd --version | head -1"
ldd --version 2>&1 | head -n 1

echo "--- loader / libc locations"
for p in /lib64/ld-linux-x86-64.so.2 /lib/x86_64-linux-gnu/libc.so.6 \
         /usr/lib/x86_64-linux-gnu/libc.so.6 /usr/lib/libc.so.6 \
         /usr/lib64/libc.so.6 /lib/libc.so.6; do
    ls -l "$p" 2>&1
done

echo "--- maximum GLIBC_2.x version node in the libc we have"
for c in /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc.so.6 \
         /usr/lib/libc.so.6 /usr/lib64/libc.so.6 /lib/libc.so.6; do
    if [ -r "$c" ]; then
        printf 'libc=%s -> ' "$c"
        # `strings` is absent from Debian/Ubuntu minimal images; `grep -a`
        # works on a binary and is present everywhere. Not byte-identical to
        # `strings` (no minimum-length filter) but sufficient for version nodes.
        grep -a -o 'GLIBC_2\.[0-9][0-9]*' "$c" 2>/dev/null |
            sort -u | sort -t. -k2,2n | tail -n 1
    fi
done

echo "--- musl loader?"
ls -l /lib/ld-musl-x86_64.so.1 2>&1

echo "--- libstdc++ / libgcc presence (C++ runtime deps)"
ls -l /usr/lib/libstdc++.so.6 /usr/lib64/libstdc++.so.6 2>&1 | head -n 2
