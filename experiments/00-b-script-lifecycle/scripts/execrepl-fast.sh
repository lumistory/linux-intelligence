#!/bin/bash
# Negative control for execrepl: exec immediately, so the whole script lives
# for about a millisecond. Expected result: the image change is NOT observed.
exec /usr/bin/ls /etc/hostname
