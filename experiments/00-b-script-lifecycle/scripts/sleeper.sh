#!/bin/bash
# Case: long sleep, killed from outside (SIGINT or SIGKILL driver-side).
echo "sleeper:pid=$$ ready"
sleep 30
echo "sleeper:UNEXPECTED-not-killed"
