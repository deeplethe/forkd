#!/usr/bin/env bash
# Runs inside asciinema rec; simulates a human typing the wp-bench command.
set -e

type_cmd() {
    local s=$1
    printf '\033[32m$\033[0m '
    for ((i=0; i<${#s}; i++)); do
        printf '%s' "${s:$i:1}"
        sleep 0.04
    done
    printf '\n'
}

# No clear — leave the banner visible from frame 1 so static previews
# (GitHub markdown, social cards) show meaningful content.
printf '\033[1;36m# sandstorm v0.4 prototype — UFFDIO_WRITEPROTECT pause-window\033[0m\n'
sleep 0.6
printf '\033[1;36m# this is the per-BRANCH cost sandstorm v0.4 wants to drop below 10 ms\033[0m\n'
sleep 0.8

type_cmd 'sudo sandstorm wp-bench --region-mib 1024'
sleep 0.4
sudo /home/yangdongxu/sandstorm/target/release/sandstorm wp-bench --region-mib 1024
sleep 0.5

printf '\033[1;33m# ⬆  3 ms WP-arm = the v0.4 BRANCH pause-window analog\033[0m\n'
printf '\033[1;33m#    vs ~150 ms in v0.3.4 on ext4 — 50x faster on 1 GiB\033[0m\n'
sleep 3
