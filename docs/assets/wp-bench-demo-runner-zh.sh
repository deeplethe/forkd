#!/usr/bin/env bash
# 中文版本 demo-runner. 跟 wp-bench-demo-runner.sh 完全对应,
# 只是顶部 / 底部的注释行换成中文. wp-bench 自己的输出保持英文
# (CLI 输出不翻译).
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

# 第一帧就显示中文 banner, README-zh.md 静态预览也有内容
printf '\033[1;36m# sandstorm v0.4 原型 — UFFDIO_WRITEPROTECT pause window 测量\033[0m\n'
sleep 0.6
printf '\033[1;36m# 这就是 sandstorm v0.4 想压到 10ms 以下的"每次 BRANCH 卡顿时间"\033[0m\n'
sleep 0.8

type_cmd 'sudo sandstorm wp-bench --region-mib 1024'
sleep 0.4
sudo /home/yangdongxu/sandstorm/target/release/sandstorm wp-bench --region-mib 1024
sleep 0.5

printf '\033[1;33m# ⬆  3 毫秒的 WP-arm = v0.4 的 BRANCH pause-window\033[0m\n'
printf '\033[1;33m#    v0.3.4 在 ext4 上是 ~150 毫秒 — 1 GiB 内存提升 50 倍\033[0m\n'
sleep 3
