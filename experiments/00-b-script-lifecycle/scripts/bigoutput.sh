#!/bin/bash
# Case: large output volume from many short-lived commands.
for i in $(seq 1 2000); do
    echo "line-$i"
    /bin/echo "ext-$i"
done
