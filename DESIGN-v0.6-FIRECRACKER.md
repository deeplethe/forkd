# Firecracker 后端（v0.6 可选后端）

状态：代码已写，**未在 KVM 上跑过**。开发和实测的三台机器（bm、bj、hk）都没有 `/dev/kvm`，本文所有行为都只经过 macOS 上的编译和单元测试。第一次上 KVM 前，下面的每一条都该当作假设。

`DESIGN-v0.6.md` 第 2 节把 Firecracker 定为可选后端，本文说明它怎么接到 v0.6 的节点上。代码在 `crates/sandstorm-core/src/node/firecracker.rs`，接口在 `backend.rs`。

## 1. 范围

v0.5 的高级功能逐条对照 v0.6 的裁决：

| 功能 | v0.6 裁决 | 本后端 |
| --- | --- | --- |
| 多个子沙箱共享 page cache（File backend，`MAP_PRIVATE` 同一 inode） | 保留 | **做了。** 同一 tag 的 N 个 fork 映射同一个内存文件 |
| diff snapshot 链 | 保留并重做 | **做了。** Firecracker 原生 Diff 快照，链在节点本地用硬链接持有，第一次恢复时合成一份并缓存 |
| 每个子沙箱私有 rootfs（reflink） | 保留（PR #321 的思路） | **做了。** 方式与 #321 不同，见第 3 节 |
| 恢复后对时 | 保留 | **做了。** 恢复后通过 agent `date -s` |
| live-fork（memfd + UFFD_WP） | 砍掉 | **不做。** 需要私改的 Firecracker 和 `--no-seccomp`，裁决理由仍然成立 |
| hugepages | 未提 | **不做。** Firecracker 的大页快照恢复只支持 UFFD backend，和共享 page cache 二选一 |
| 用户卷（`volume`） | — | **还不支持**，请求带卷时直接报错。runsc 是 bind 一个宿主机目录，VM 里需要块设备，另做 |

## 2. 接口：后端接管"准备"

`Backend` trait 原先只管 boot / restore / checkpoint / remove / kill / exec，沙箱的可写层和 OCI bundle 由 `Runtime` 自己做。对 Firecracker 这行不通：VM 要的是一个块设备镜像，不是 overlay 目录。所以 trait 多了四个方法：

- `template_root(name)`：模板在节点上的样子。runsc 是 `templates/<name>/rootfs/` 目录，Firecracker 是 `templates/<name>/rootfs.ext4` 文件。
- `prepare(req)`：建可写层，写 `bundles/<id>/config.json`。runsc 照旧是 overlay + OCI spec；Firecracker 是 reflink 一份 ext4，加一份 VM 配置。
- `layer(id)`：detach 时打包的目录。
- `unmount(id)` / `teardown(id)`。

`bundles/<id>/config.json` 存在仍然是"这个沙箱在本节点"的标志，`observe` 和 `branch` 不需要知道是哪个后端。

不变的约定（`backend.rs` 模块文档）：沙箱的所有进程都在 `/sys/fs/cgroup/sandstorm/<id>` 里。Firecracker 进程启动前就把自己写进这个 cgroup，所以 frozen 档（`cgroup.freeze`）、内存记账、`cgroup.kill` 兜底对两个后端是同一套代码。

## 3. 镜像：内存和磁盘必须一起走

guest 的 page cache 引用的是磁盘块。用 A 时刻的内存配 B 时刻的磁盘，文件系统会被悄悄写坏。v0.5 的 `sandstorm-init.sh` 把 `/tmp` 挂成 tmpfs，注释里就记着多个子沙箱共用一个 ext4 时出现的 "Structure needs cleaning"。

所以每一份镜像（tag、snapshot 档、warm 模板）都自带一份 rootfs，在 VM 暂停的状态下 reflink 出来：

```text
<image>/
  vmstate        Firecracker 的设备和 vCPU 状态
  mem.0          完整内存（Full 快照）
  mem.1 .. mem.k 依次的 Diff 快照，稀疏文件，只有脏页
  mem.assembled  第一次恢复时合成，之后所有 fork 共享这个 inode
  rootfs.ext4    暂停时的磁盘，reflink
```

**磁盘路径是相对路径。** boot 时 rootfs 的 `path_on_host` 写成 `rootfs.ext4`，Firecracker 以沙箱自己的 `layers/<id>/disk/` 为工作目录启动。vmstate 里记下的是这个相对路径，所以任何一个子沙箱在 `snapshot/load` 时打开的都是它自己目录下的那份，从来碰不到父沙箱的磁盘，也不需要父沙箱的路径在本机存在（跨节点拉来的 tag 也一样）。之后仍按 #321 的顺序做一次 `resume_vm: false` → `PATCH /drives/rootfs`（绝对路径）→ `PATCH /vm Resumed`，作为第二道保险；任何一步失败都直接杀掉，不 resume。

reflink 是硬要求（`DESIGN-v0.6.md` 7.5）：后端启动时在 work_dir 上试一次 `FICLONE`，失败就拒绝启动，不退化成全量拷贝。

## 4. diff 链

- boot 和每次 load 都打开脏页跟踪（`track_dirty_pages` / `enable_diff_snapshots`）。
- 恢复时把镜像的 `mem.*` 硬链接到 `layers/<id>/base/`。这就是这个 VM 当前内存的来源。
- checkpoint（snapshot 档、branch、warm seed）时：如果 `base/` 有链且长度小于 8，做 Diff 快照，新镜像里硬链接 `base/mem.*`，再加一个 `mem.<k+1>`；否则做 Full，链重新开始。
- 硬链接而不是指针：每份镜像都自包含，`drop_tag` 删一个 tag 不会弄坏从它派生的 tag，主服务也不需要知道 tag 之间的父子关系。代价是 tag 跨节点传输时链会作为普通文件全部传过去。
- 合成：reflink `mem.0`，再按 `SEEK_DATA`/`SEEK_HOLE` 把每个 diff 的数据段写上去，tmp + rename。snapshot 档的镜像恢复后就删，tag 的镜像里缓存的 `mem.assembled` 会一直留着给后面的 fork 用。

收益在 snapshot 档：一个沙箱反复 hot ↔ snapshot，第二次起每次只写脏页。`DESIGN-v0.6.md` 第 13 节记的是每个 checkpoint 50.2 MiB（runsc，整份写），Firecracker 下的实际数字要实测。

## 5. 网络

v0.6 每个沙箱一个 netns，里面是 veth `sst<tag>s`，地址 10.86.x.y。VM 的地址在 guest 里，所以 netns 里再加一层：

```text
netns:  sst<tag>s 10.86.x.y   tap0 10.42.0.1/30  →  guest eth0 10.42.0.2
        DNAT: 非 tap0 进来的包 → 10.42.0.2
        MASQUERADE: 10.42.0.2 出去的包 → 10.86.x.y
```

**每个 guest 都是 10.42.0.2。** 快照里的网络配置因此在任何 netns 里都对，fork 和跨 netns 恢复不需要改 guest 的地址。v0.5 的 `netns-setup.sh` 是同一个思路。节点、gateway、warm seed 继续连 10.86.x.y，看不出后端的区别。

## 6. guest

- PID 1 仍是 `rootfs-init/sandstorm-init.sh` → `sandstorm-agent.py`（8888 端口）。
- 模板的 `args` / `env` / `cwd` 通过内核命令行 `sandstorm.app=<base64 JSON>` 传进去，agent 启动时拉起它。x86 的命令行上限是 2048 字节，超了就在 prepare 时报错。
- `exec` 走 agent 的 JSON 协议，连 10.86.x.y:8888。**8888 因此被 agent 占用**，模板不能用这个端口。
- `{{PORT}}` 替换、`memory_max_mib` 和 runsc 后端语义相同。guest 内存 = `memory_max_mib`（缺省 512），cgroup `memory.max` 再加 64 MiB 给 VMM 自己。

## 7. 进程生命周期

- Firecracker 以 `setsid` 启动，在 `ip netns exec` 下运行，不是 sandstormd 的会话成员。sandstormd 重启不影响它，`observe` 照旧从 cgroup 读出它。
- API socket 在 `bundles/<id>/fc.sock`，pid 在 `bundles/<id>/fc.pid`。后端不持有任何内存状态，所有操作都从 id 推出路径。
- 杀进程前核对 `/proc/<pid>/cmdline` 里有这个沙箱的 socket 路径，防止 pid 复用误杀。

## 8. 已知风险，要在 KVM 上先验证的

1. **frozen 档是 cgroup freeze，不是 Firecracker 的 Paused。** 冻住的 vCPU 线程看不到时间流逝，解冻后 guest 时钟落后，长时间冻结可能触发 RCU stall 告警（`DESIGN-v0.6.md` 第 4、13 节）。解冻后要不要也对时，要实测。
2. **`enable_diff_snapshots` 在新版 Firecracker 里改名为 `track_dirty_pages`。** 现在发的是旧名，新版仍接受但已标记弃用。
3. **Diff 快照的基线。** 假设 load 时开着脏页跟踪，Firecracker 的 Diff 只含 load 之后弄脏的页。如果实测不是这样，链会少页，guest 内存会坏。这是第一条要验证的。
4. **tag 跨节点传输带着整份 rootfs.ext4。** 逻辑大小可能是 GiB 级，稀疏和 reflink 在传输里都不保留。
5. **`prefetch_snapshot` 会把 rootfs.ext4 也读一遍。** 对 runsc 只有内存镜像，对这里是白读磁盘。
6. 4 档的数字全部欠着：gVisor 和 Firecracker 的内存记账不同（file/shmem 对 anon），runsc 的结论不能套过来。
