#!/bin/bash
# /forkd-init.sh — PID 1 inside the guest. Mounts pseudo-fs, then launches
# the Python agent (which warms state into memory and listens on :8888).

mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null

echo "forkd-init: launching agent..."
exec /usr/bin/python3 /forkd-agent.py
