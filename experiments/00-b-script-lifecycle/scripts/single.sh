#!/bin/bash
# Case: one command. The simplest possible script.
echo "single-case-start"
printf 'value=%s\n' "$((6 * 7))"
echo "single-case-end"
