<!-- Exported from the collaborative version at
     https://claude.ai/artifact/16AJtoKPSjvjTK9gobRFrR (rev 48, 2026-10-24).
     That doc is the source of truth; edit it there, then re-export.

     Sections 4, 12, 15 and 16 describe the gVisor/runsc implementation and
     carry measurements taken on bm. Sections 2, 3, 6 and 7 still describe
     the Firecracker/jailer design they replaced, and so contradict both the
     later sections and the code in crates/forkd-core. Reconciling them is a
     design decision, not an export artefact. -->

# forkd v0.6 架构 RFC

Oct 22, 2026 · @Wayland · 状态：草案，待讨论

## 1. 背景与目标

v0.6 把 forkd 从"单机 fan-out 工具"改造成"可大量超卖的沙箱平台"：空闲沙箱不占 CPU 和 RAM，控制器重启不丢沙箱，磁盘和网络在每个子沙箱之间真正隔离。

**v0.5 做对了什么。** 后端用上游 Firecracker 而不是 fork VMM；所有子沙箱 `MAP_PRIVATE` 同一个 `memory.bin` inode，page cache 真共享；guest agent 空闲时阻塞在 accept 上，没有定时器；auth 常量时间比较、audit 日志 SIGHUP 轮转、hub pack 格式、薄 SDK。这些全部保留。

**v0.5 的问题。** 对照 TencentCloud/CubeSandbox 的审查，forkd 踩的是同一类坑，只是代码量小十倍、改得动：

- 控制器状态是内存 map 镜像到一个 JSON 文件，重启时 `kill_orphans` 杀光所有活着的 Firecracker，运行期无人看管（issue #301）。
- 所有子沙箱以 rw 打开同一个 rootfs，N 个 guest 内核写一个 ext4，静默损坏（issue #317）。
- 生命周期只有 running 和"快照落盘再 kill"，没有冻结在内存、没有回收到 zswap 的中间档。cgroup 只写 `memory.max`，超限即 OOM kill 整个 VM。
- netns 池由脚本预建，`10.43.0.$((i+1))` 过 254 即非法，所有子沙箱同 IP 同 MAC 挂一个 bridge，无 egress 策略，guest 能直接打到宿主机。
- live-fork 需要 Firecracker 三处私改加 `--no-seccomp`，每个子沙箱整份拷贝 memory.bin 且记在控制器的 cgroup 上，write-protect 快照存在一致性竞争。
- chain spawn 每次全量 SHA-256 并生成私有 assembled 文件，深度 3 时 spawn 1.6 秒，且同 chain 沙箱之间零共享。
- Firecracker 以 root 加 ambient `CAP_SYS_ADMIN` 运行，jailer 装了但没用；CI 从未启动过一台真 VM。

**目标。** 一台 2 核 4G 机器稳定承载 250 个沙箱（约 15 hot、60 frozen、200 cold），一千个空闲沙箱的宿主机 CPU 低于 1 核，frozen 恢复 P99 低于 1 毫秒，控制器重启零沙箱丢失。

**非目标。** 共享内核后端的生产化、E2B 完整兼容、live BRANCH 的 56 毫秒 pause 窗口。**多节点调度从非目标移出**：v0.6 不实现它，但必须把迁移域（池）写进数据模型，理由在第 15 节。跨节点恢复对宿主机的强约束是实测出来的，不是设计选择，后补进去代价很大。

## 2. 结论总览：保留、重做、砍掉

保留的是已经被验证正确的地基，重做的是与超卖目标直接冲突的四块，砍掉的是收益抵不过维护代价的部分。裁决列是下拉，讨论时可以直接改。

| 模块 | 现状（v0.5） | 裁决 | 理由 |
| --- | --- | --- | --- |
| Firecracker 上游版为后端 | `forkd-vmm` 调用 Firecracker API | 保留 | 不 fork VMM 是正确路线，升级到上游最新稳定版 |
| File backend 内存共享 | 所有子沙箱 MAP\_PRIVATE 同一 inode | 保留 | page cache 真共享，是 cold 档的基础 |
| guest agent | Python PID 1，空闲阻塞在 accept | 保留 | 无定时器，比 envd 干净；补 SIGCHLD 收尸和 stream 轮询改 epoll |
| auth / audit / TLS | 常量时间比较、SIGHUP 轮转 | 保留 | 做对了，补 remote addr 和 principal 字段 |
| hub / pack 格式与 SDK | tar.zst + manifest，薄客户端 | 保留 | 格式中立，SDK 只需对齐 API |
| 控制器状态模型 | 内存 map + 整文件重写 JSON，重启杀光 | 重做 | sqlite 真相源 + reconcile + 重启重新接管（第 5 节） |
| 生命周期 | 只有 running 和快照后 kill | 重做 | 新增 frozen / cold 两档，是 v0.6 的核心（第 4 节） |
| 进程隔离与 cgroup | root + ambient caps，只写 memory.max | 重做 | jailer + per-VM scope + memory.high / cpu.weight / freezer（第 6 节） |
| rootfs | 所有子沙箱 rw 共用一个 ext4 | 重做 | 只读 base + per-child 可写层（第 7 节） |
| 网络 | 脚本预建 netns 池，254 上限，无策略 | 重做 | 控制器动态建 netns + nftables 默认 deny（第 8 节） |
| chain 校验与 assembled 文件 | 每次 spawn 全量哈希、私有文件 | 重做 | 写入时哈希一次，assembled 按 chain head 缓存成共享 inode |
| CI | ubuntu-latest，KVM 测试全部 ignore | 重做 | 自托管 KVM runner，每个 PR 启动真 VM |
| live-fork（memfd + UFFD\_WP） | 需 Firecracker 私改三处、--no-seccomp | 砍掉 | 零共享、cgroup 记错、wp 快照有竞争；回到原生 diff snapshot |
| vendored Firecracker fork | v1.12 加 5 个提交 | 砍掉 | 随 live-fork 一起删，回到上游二进制 |
| `forkd-uffd` crate 与 experiments/ | PoC 和 no-op stub | 砍掉 | 代码已无用途，历史在 git 里 |
| CLI 直连 Firecracker 路径 | fork / run / snapshot 绕过 daemon | 砍掉 | 两套控制面互相冲突，CLI 改为纯 daemon 客户端 |
| workspace 子系统 | 独立 spawn 路径，suspend 会丢数据 | 砍掉 | 其语义被四档生命周期覆盖，并入 sandbox 资源 |

## 3. 目标架构

[embedded content: v0.6 组件与进程模型 · 1 个控制器，每沙箱 4 层]

控制器是唯一的常驻进程，它不持有任何 VM 的生命周期：每个沙箱是一个独立的 systemd scope 里的 jailer 加 Firecracker 进程树，控制器挂了它们照常跑，重启后按 sqlite 里的记录按 API socket 重新接管。与 v0.5 相比删掉的是：vendored Firecracker、`forkd-uffd`、CLI 直连路径、netns 预建脚本、workspace 子系统。

## 4. 生命周期四档

超卖的全部来源是"没在干活的沙箱不占 CPU 也不占 RAM"。每个沙箱在任一时刻处于三档之一。本节的数字来自 2026-10-23 在一台 2 vCPU / 3.6 GB 云主机上对 gVisor（runsc release-20261012.0）的实测，载荷为 python + numpy 加一块 32 到 48 MiB 的 float64 数组。

**原本设计的四档中的 cold 档已经删除**，理由在第 13 节。简要来说：它在单沙箱上成立（释放 90%，耗时 7 ms），但只要多个 cgroup 同时要求回收就全面失败，批量、串行、限流三种策略在 8 沙箱下的达标率均为 0/8。

| 档位 | 位置 | CPU | 本节点 RAM | 本节点磁盘 | 恢复延迟 |
| --- | --- | --- | --- | --- | --- |
| hot | 某节点 | 实际用量，`cpu.weight` 超卖 | 63 MiB | 可写层 | 0 |
| frozen | 同节点，`cgroup.freeze=1` | **实测 0 µs** | 不变 | 可写层 | 8–11 ms |
| snapshot | 同节点本地盘，`runsc checkpoint` | 0 | **0** | 快照 50 MiB 加可写层 | 84 ms（8 并发 p50 465 ms） |
| **detached** | **只剩卷** | 0 | 0 | **0** | 冷启动加解归档，约 11 s |

前三档都在**同一个节点**上，可写 rootfs 始终在本地盘。detached 是唯一离开节点的档位，也是真正提供弹性的那一档：节点要维护或挂掉，上面的沙箱全部降到 detached，换台机器重来，文件从卷里来。

关键数字：8 个沙箱全量 checkpoint 并销毁共耗 **1.1 秒**，归还宿主机内存 **413 MiB**，成功率 8/8；落盘每沙箱 50.2 MiB（`--compression=none`，压缩不划算：flate 只省 20% 但 checkpoint 慢 3 倍、restore 慢 2 倍）。

关键数字：8 个沙箱全量 checkpoint 并销毁共耗 **1.1 秒**，归还宿主机内存 **413 MiB**，成功率 8/8；落盘每沙箱 50.2 MiB（`--compression=none`，压缩不划算：flate 只省 20% 但 checkpoint 慢 3 倍、restore 慢 2 倍）。

**降档由两件事驱动。** 一是空闲时长：最后一次 agent 请求完成后（包括流式请求的仍在进行中，这是 Cube 和 v0.5 都做错的地方）超过 `idle_to_frozen`（默认 30 s）和 `frozen_to_snapshot`（默认 5 min）。二是节点压力：`/proc/pressure/memory` 的 some avg10 超过阈值时，生命周期管理器按"最久未活跃优先"把 frozen 沙箱落盘成 snapshot，直到压力回落。降档是异步后台任务，跑在 SCHED\_IDLE。实测 checkpoint 单个 41 ms、串行 8 个 1.1 秒，这个速度下降档不需要复杂的限流，但仍应按磁盘带宽设并发上限。

**升档由请求到达触发。** API 收到面向某沙箱的 exec / eval / 端口流量时，先把它升到 hot 再转发。frozen 到 hot 是一次 `cgroup.freeze=0`，实测 8 到 11 ms；snapshot 到 hot 是一次 `runsc restore`，单个 84 ms。并发唤醒要限流：实测 8 个同时 restore 的 p50 升到 465 ms、max 491 ms，矶点在磁盘读取带宽，所以同一秒内允许的 restore 数量要按设备吞吐配，超出的排队。`runsc restore --background` 的懒加载尚未测试，是压低这个延迟的主要手段。

**两个细节。** frozen 档解冻前控制器通过 agent 做一次对时，并在 Firecracker 后端上通知 guest 被暂停过（`KVM_KVMCLOCK_CTRL`），避免 guest 软锁定和 RCU stall 告警；guest 内开着的 TCP 连接在冻结和落盘期间会被对端超时，这是接受的行为，文档要写明。

[embedded content: 四档状态转移 · 前三档同节点，detached 跨节点]

降档只能逐级，升档总是一步到 hot。kill 从任何档位直接删除。

**两个容易误解的性质。** frozen 不释放内存，它和 hot 占用同一份内存预算，**内存密度来自 snapshot 档**；snapshot 不释放本地磁盘，可写层还在，**磁盘密度来自 detached 档**。两个预算独立，分别在第 7.4 节算过。

**为什么不做跨机状态迁移。** 第 15 节的实测证明跨机 checkpoint 在指纹完全一致时仍会 SIGSEGV。而且产品上也不需要：价值主要在磁盘，内存只要跑的时候别断就行。所以 detached 走冷启动加挂卷，丢弃内存状态，这在 API 语义上要写明。

## 5. 控制面与状态模型

改成"sqlite 是唯一真相源，进程是 reconcile 的输入"。控制器重启、崩溃、升级都不碰任何沙箱进程。

**状态存储。** `state.json` 换成 sqlite（WAL 模式），三张表：`sandboxes`（id、tag、档位、期望档位、scope 名、API socket 路径、netns、可写层路径、最后活跃时间、pid + 启动时间、boot id）、`snapshots`（tag、parent\_tag、内容哈希、状态）、`jobs`（长操作）。每次变更是一个事务，不再整文件重写，不再在全局锁里做 I/O。

**Reconcile 循环。** 一个后台任务每 5 秒做一次，外加每次写入后立即触发一次：读 sqlite 里的期望状态，读实际状态（systemd scope 是否存在、Firecracker API socket 是否应答、cgroup.freeze 值、memory.current），差异就是工作项。进程死了而数据库说 hot，标记为 `failed` 并清理 scope、netns、可写层；scope 存在而数据库没有记录，按 scope 名里的 boot id 判断是本机本次启动的孤儿才杀。这把 v0.5 里散在 `reconcile()`、`kill_orphans()`、`warn_orphan_firecrackers()` 里的三套逻辑合成一套。

**重启重新接管。** 启动时不杀任何进程。对每条 `sandboxes` 记录，连接其 API socket，`GET /vm` 拿到 Firecracker 自己报的状态，写回数据库。这就是为什么 VM 必须在自己的 systemd scope 里：它们不在控制器的 cgroup 下，`KillMode=control-group` 不会连带杀掉它们。控制器本身不再持有 `Child` 句柄，用 pidfd 观察退出事件。

**API 作业模型。** 任何可能超过 1 秒的操作（spawn n > 10、snapshot、compact、cold 到 hot 的批量唤醒）返回 202 和 `job_id`，`GET /v1/jobs/{id}` 查进度，每个子项独立成败，不再 all-or-nothing。`POST` 接受 `Idempotency-Key` 头，重试不产生重复沙箱。错误体带稳定的 `code` 字段，删掉所有按错误字符串判断重试的代码。资源收敛为 `snapshots`、`sandboxes`、`jobs` 三个，`workspaces` 并入 `sandboxes`（一个带 `name` 和 `persist: true` 的沙箱）。

**并发模型。** 控制器里不再有 `live_vms` 这样的全局锁。每个沙箱一把小锁，所有阻塞 I/O（哈希、拷贝、remove\_dir\_all、进程等待）走 `spawn_blocking`，并且有超时。

## 6. 隔离与进程模型

每个沙箱是一条 `systemd scope → jailer → firecracker` 进程链，以非 root 身份跑在自己的 chroot、netns 和 cgroup 里。v0.5 装了 jailer 但没用，Firecracker 以 root 加 ambient `CAP_SYS_ADMIN` 运行，这是和 CubeSandbox 同样的错误。

**启动链。** 控制器用 `systemd-run --scope --unit=sb-<id>` 起 jailer，jailer 参数：`--uid/--gid` 为每沙箱独立的高号 uid，`--chroot-base-dir` 下只有 memory.bin、vmstate、rootfs base、可写层和 vsock socket 的硬链接，`--netns` 指向控制器刚建的命名空间，`--cgroup-version 2`。控制器自身不需要 `CAP_SYS_ADMIN`，因为不再 setns 进 guest 网络去连 agent，改走 vsock。

**cgroup 写什么。** 每沙箱 scope 里写四项：`memory.high` 设为配额，超过触发回收而不是杀进程；`memory.max` 设为配额的 1.5 倍作为最后防线；`cpu.weight` 按档位设（hot 100，其它 1），`cpu.max` 只在用户指定硬顶时写；`pids.max` 默认 512。降档和升档只改 `cgroup.freeze`、`cpu.weight` 和写 `memory.reclaim`，不重建 cgroup。

**共享页的记账。** memory.bin 的干净页记在第一个缺页的沙箱的 cgroup 上，这是内核行为。解法是控制器在注册一个 tag 时用一个长期存在的 `tag-<name>` cgroup 预读一遍文件（`cat > /dev/null`），把共享页记到 tag 级，沙箱的 `memory.current` 就只包含它自己的脏页。这也是调度器能按"每沙箱真实增量"做准入的前提。

**宿主机内核参数。** `kvm.halt_poll_ns=0`，`transparent_hugepage=never`（对 guest 内存；2 MB 页会毁掉 CoW 共享），`zswap.enabled=1`，`zswap.compressor=lz4`，`zswap.max_pool_percent=50`，开 PSI。`setup-host.sh` 里的 KSM 和 1 GiB hugetlb 预留删掉。Firecracker 启动不再传 `track_dirty_pages: true`，只在用户请求 diff snapshot 前通过 `PATCH /vm` 打开。

**seccomp。** Firecracker 默认过滤器一直开着，没有任何路径需要 `--no-seccomp`。这是砍掉 live-fork 的直接收益。

## 7. 存储：rootfs、卷与快照链

存储分两层，职责截然不同：**本地盘跑得快但跟着节点走，网络卷活得久但小文件慢**。本节的数字来自 2026-10-23 在 bm（2 vCPU / 3.6 GB）上的实测，卷的组合是 JuiceFS 1.4.2 加 PostgreSQL 18.6 元数据加 RustFS 1.0.3 对象存储。

rootfs 改成"只读 base 加 per-child 可写层"，快照链改成"写入时哈希一次、按 head 共享 assembled 文件"。两件事分别修 issue #317 和 chain spawn 的 1.6 秒。

**rootfs。** 采用 PR #321 已经验证过的机制：每个子沙箱 spawn 时控制器在 XFS 或 btrfs 上 `FICLONE` 一份 tag 的 rootfs 作为它的私有 backing，`PUT /snapshot/load` 带 `resume_vm: false` 让 guest 停在未执行状态，`PATCH /drives/rootfs` 把根盘指到这份 backing，再 `PATCH /vm Resumed`。顺序是关键：guest 在被重新指向之前没有执行过任何指令，所以日志回放、agent 启动都碰不到共享的 base。#321 在 v1.5.1 到 v1.17.0 的上游 Firecracker 上都实测写入落到新文件，不需要 vendored fork，也不需要重烤快照。重新指向失败的子沙箱不 resume 而是直接销毁，宁可少一个沙箱也不让它跑在共享 base 上。实际上什么都不拷，spawn 延迟不变。不用 overlayfs，因为那需要 guest 内两个块设备加 overlay 逻辑。宿主机不支持 reflink 时拒绝启动并报错，不默默退化成全拷贝（v0.5 的 `chain::reflink_copy` 就是这样退化的，而且 FICLONE 号还写错过）。`Snapshot` 元数据记录 rootfs 路径、只读标志和内容哈希；#321 已经让 daemon 烤的快照写入 `rootfs` 字段。可写 volume（`/dev/vdb`）仍是同一个缺陷，#321 明确列为范围外，v0.6 用同样的 reflink 加 re-point 处理。

**可写层的生命周期。** 跟沙箱走：snapshot 档保留，kill 时删除。`persist: true` 的沙箱允许把可写层导出为新的 base（这就是 v0.5 workspace 的全部语义）。

**memory.bin。** 保持 File backend 加 `MAP_PRIVATE`。Firecracker 启动时不开 dirty tracking。用户请求 diff snapshot 时才打开，拍完关掉。

**快照链。** 每个 snapshot 写入完成时计算一次 SHA-256，文件按 `<hash>` 存放，tag 是指向 hash 的指针。spawn 时不再哈希，只比较 inode、size、mtime 和记录值；要更强的完整性就开 fs-verity。chain head 的 assembled 文件第一次 spawn 时生成，存在 `<hash>/assembled.bin`，之后所有同 head 的沙箱映射同一个 inode，page cache 共享恢复。工作目录不再放 `/tmp`，因为 tmpfs 上既不能 reflink 也不该放 GB 级文件。

**删掉的 fallback。** v0.5 在上一次 branch 的内存文件丢失时会回退到源 tag 的 memory.bin 并只打一条 warning，产出的镜像是错的。v0.6 直接报错。

### 7.1 卷：跨机耐久的那一层

超卖的价值不在内存里，在磁盘上。沙箱跑的时候内存别断就行，但用户的代码和数据不能丢。所以跨机连续性靠**卷**，不靠状态迁移（第 15 节的实测证明跨机 checkpoint 不可靠）。

**挂载粒度是选型的决定性标准。** 云盘要按沙箱挂载，挂载要几秒到几十秒且每实例有挂载数上限（20 到 40 个），而实测单机能跑 46 个 hot 沙箱，装不下。网络文件系统每节点只挂一次，沙箱的卷就是子目录加 bind mount，**挂载彻底离开关键路径**。

**实测：gVisor 下可用。** JuiceFS 目录 bind mount 进 runsc 沙箱正常，沙箱 141 ms 启动，能读到宿主机写的文件，FUSE 叠 gofer 两层用户态文件系统没有冲突。沙箱销毁重建后数据完好，这正是 detached 档依赖的性质。

## 8. 网络

控制器在 spawn 时动态创建 netns，用 nftables 做 NAT 和默认 deny 的出站策略，不再有预建池、254 上限和 bridge。不重写 conntrack，内核处理分片、ICMP 和 PMTU。

**拓扑。** 每沙箱一个 netns `sb-<id>`，里面一个 tap 给 Firecracker，一对 veth 连回宿主机命名空间。宿主机侧不建 bridge，veth 的宿主机端直接分配 /30，每沙箱自己的点对点链路，互相天然不可达。guest 内 IP 和 MAC 保持固定（`10.42.0.2`，`AA:FC:00:00:00:01`），这是 v0.5 已经做对的：clone 不用重新配 IP，身份全在宿主机侧。

**地址分配。** 宿主机侧从 `100.64.0.0/10` 按 /30 切，容量 400 万，分配记在 sqlite 里，reconcile 回收死沙箱的段。

**策略。** 宿主机侧一张 nftables 表 `forkd`，forward 链默认 drop；每沙箱一个命名 set，允许的目标 CIDR 放进去，规则是 `iifname "veth-sb-*" ip daddr @allow-<id> accept`。固定前置一条 drop 覆盖 `10/8`、`172.16/12`、`192.168/16`、`169.254/16`、`100.64/10` 和宿主机自己的地址，用户策略打不开它。`allow_internet: true` 就是往 set 里放 `0.0.0.0/0`。出站 masquerade，MSS clamp 到 PMTU。IPv6 在 netns 里禁用，启动时断言。

**进站。** 端口暴露是每沙箱一条 DNAT，宿主机端口由控制器分配并记录。进站请求到达时先升档：这需要请求先经过控制器或一个轻量的 L4 代理。v0.6 先做前者，即只有走 API 的 exec / eval 触发升档；cold 沙箱的暴露端口返回连接拒绝，文档写明。

**控制通道。** agent 改走 vsock，控制器连 `<chroot>/v.sock` 再 `CONNECT 8888`。删掉 setns 线程、`CAP_SYS_ADMIN` 依赖和共享 tap 模式下本机任意用户都能连到 agent 的问题。

**实现。** 用 netlink（`rtnetlink` crate）和 `nftables` crate 直接说话，不 shell out 到 `ip` 和 `nft`。每沙箱 netns 创建约 2 ms，在 spawn 路径上不是瓶颈。

## 9. Guest 侧

guest 的目标是空闲时零唤醒、恢复后身份正确。agent 的大方向不变，内核配置从"下载 Firecracker CI 的 vmlinux"改成仓库内固定。

**内核。** 配置文件进仓库，CI 构建并按哈希发布。关键选项：`NO_HZ_FULL`、`HZ_100`、`VMGENID`、`VIRTIO_BALLOON` 加 `PAGE_REPORTING`、`HW_RANDOM_VIRTIO`、`RANDOM_TRUST_CPU`，关掉 `DEBUG_FS`、`KPROBES`、`MODULES`。启动参数加 `nohz_full=all`、`rcu_nocbs=all`。一个内核给所有模板，快照元数据记内核哈希，不匹配拒绝 restore。

**agent。** 保留 Python PID 1 的结构，但补三件事：SIGCHLD 处理和 `waitpid(-1)` 循环收尸（v0.5 的孤儿进程会堆成僵尸）；stream 会话的两个 0.5 秒 select 循环改成 epoll 阻塞，否则一个开着的 shell 每秒 4 次唤醒；监听从 TCP `0.0.0.0:8888` 换成 vsock 端口 8888。长期看这个 agent 应该换成 Rust 单二进制，但不在 v0.6 范围。

**恢复后的身份。** Firecracker 默认挂 vmgenid 设备，v0.6 在 `forkd doctor` 里验证 guest dmesg 出现 `vmgenid` 且 restore 后 `/proc/sys/kernel/random/boot_id` 变化。内核 CRNG 由 vmgenid 重播；用户态随机数（Python `random`、numpy、OpenSSL）在每个 clone 里是相同的，agent 在收到恢复通知时调用 `random.seed()` 和 `np.random.seed()` 重播自己的进程，其它进程的这个问题写进文档作为已知限制。恢复通知走 vsock 一条消息，换掉 v0.5 的对时 ping 循环，消息里带宿主机时间，agent `clock_settime` 一次。hostname 设为沙箱 ID。

**模板快照的时机。** bake 不再 sleep 固定 `boot_wait_secs`，而是等 agent 发来 `warmup_ready` 再拍。模板里不跑任何带定时器的常驻进程；recipe 的 warmup 进程（比如 Chromium）如果必须常驻，在 recipe README 里标注其空闲 CPU 成本。

## 10. 安全默认值与测试

默认值在开源后几乎改不动，v0.6 是最后一次能把它们定对的机会。原则：不配就是安全的，开放要显式写。

- **API：** token 文件不存在时拒绝启动，而不是默认开放；默认监听 Unix socket，TCP 要显式配。沙箱 ID 改为 128 位随机，删掉计数器。
- **宿主机路径：** `POST /v1/snapshots` 不再接受任意 `kernel` / `rootfs` 路径，只接受已注册的 tag 或 hub 引用。本地文件导入是 CLI 的事，走 `forkd import`。
- **能力：** 控制器 systemd 单元只要 `CAP_NET_ADMIN`（建 netns 和 nftables）和 `CAP_SYS_RESOURCE`（cgroup），不要 `CAP_SYS_ADMIN`。Firecracker 由 jailer 降权。
- **MCP：** 所有拼进 guest shell 的参数走 `shlex.quote`，`wait_for_text` 的 path 注入直接修。
- **网络：** 出站默认 deny，`allow_internet` 默认 false。
- **audit：** 补 remote addr 和 token 指纹，401 也记。

**CI 必须启动真 VM。** 一台自托管 KVM runner（裸金属或开了嵌套的云主机），每个 PR 跑：spawn 1 和 spawn 100、四档升降各一次、控制器 kill -9 后重启且 100 个沙箱全部存活、两个子沙箱写同一路径互不影响、netns 里 ping 宿主机被拒绝。`#[ignore]` 的 KVM 测试全部解除 ignore，在这台 runner 上跑。`cargo test` 仍在 GitHub 托管 runner 跑单元测试。没有 KVM runner 就不发版。

## 11. 从 v0.5 迁移与改名

v0.6 是架构重做但保留名字，改名放到 1.0 作为唯一一次兼容性 break。两件事分开做，否则用户分不清哪些是改名带来的、哪些是架构带来的。

**v0.6 兼容性。** `/v1/sandboxes`、`/v1/snapshots` 请求体保持可用，新增字段（`tier`、`idle_policy`）有默认值。删掉的接口：`/v1/workspaces/*`（改用带 `name` 的沙箱）、branch 的 `mode: "live"` 和 `wait`、`live_fork` 参数。返回 410 并在错误体里给出替代调用。hub 上已发布的 9 个快照继续可用：它们的 rootfs 是 rw 烤的，控制器读到无只读标志的旧快照时自动用 reflink 可写层包一层，行为和新快照一致。

**状态迁移。** 启动时发现 `state.json` 就导入 sqlite 然后改名为 `.imported`。旧记录里的 pid 不被信任，按 reconcile 规则处理。

**改名的成本。** 仓库里 3386 处引用、227 个文件。不疼的：API 路径是 `/v1` 中立的。疼的：烤进每个已发布快照里的 `/forkd-init.sh`、`/forkd-agent.py`、`/etc/forkd-recipe.env`，以及 PyPI `forkd`、npm `@deeplethe/forkd`、`FORKD_URL` / `FORKD_TOKEN` 环境变量、`/var/lib/forkd` 等路径。

**改名的做法（1.0）。** guest 内路径新旧并存一个大版本，kernel cmdline 的 `init=` 由快照元数据决定；环境变量新名优先、旧名警告；PyPI 和 npm 旧包发最后一版做转发。名字本身不在本 RFC 范围。

## 12. 里程碑 M0 验收指标

M0 只回答一个问题：在一台 2 核 4 GB 的机器上，这套设计能不能把沙箱的空闲成本压到接近零。下表标 ✓ 的已在 2026-10-23 实测（bm，gVisor runsc release-20261019.0，python+numpy 载荷），标**未测**的是 M0 剩下的工作。脚本进 `bench/m0/`，结果进 README。

| 指标 | 验收值 | 状态 |
| --- | --- | --- |
| hot 每沙箱宿主机 RAM | ≤ 70 MiB | 已测 63 MiB ✓ |
| frozen 期间 CPU | ≈ 0 | 已测 0 µs ✓ |
| frozen 到 hot | P99 ≤ 20 ms | 已测 8–11 ms ✓ |
| snapshot 到 hot，单个 | P99 ≤ 150 ms | 已测 84 ms ✓ |
| snapshot 到 hot，8 并发 | p50 ≤ 500 ms | 已测 465 ms ✓ |
| 批量降 snapshot | 8 个 ≤ 3 s | 已测 1.1 s，8/8 成功 ✓ |
| 每 parked 沙箱磁盘 | ≤ 150 MiB | 已测 127 MiB（含 50 MiB 快照）✓ |
| reflink 零拷贝 | 克隆不占磁盘 | 已测 10 份 1 GiB 模板 → 0 MiB ✓ |
| 卷大文件写入 | ≤ 本地盘 2 倍 | 已测 636 对 554 ms ✓ |
| 依赖归档往返（3 万文件） | ≤ 30 s | 已测 16.1 s（zstd -3）✓ |
| 1000 个空闲沙箱的宿主机 CPU | ≤ 3%（两核合计） | **未测**，28 个时为 0.6% |
| detached 到 hot | ≤ 15 s（300 MiB 依赖） | 已测 1.7–2.8 s，文件校验 3/3 ✓ |
| 控制器 kill -9 后重启 | 0 沙箱丢失，接管 ≤ 10 s | 已测 20/20 存活，token 不变，接管 134 ms ✓ |
| 磁盘隔离 | 100 沙箱并发写同路径互不影响 | **未测**（issue #317） |
| 网络隔离 | ping 宿主机、邻居、169.254.169.254 均失败 | **未测** |
| spawn 延迟不退化 | P50 ≤ 59 ms 加 20% | **未测** |
| chain 深度 3 的 spawn | P50 ≤ 100 ms（v0.5 是 1668 ms） | **未测** |

两个数字有意放宽：hot 档 20 MB 而不是 10 MB，因为 agent 还是 Python；cold 档 100 ms 而不是 50 ms，因为 2 核机器上 zswap 解压会和其它事排队。M0 之后按实测收紧。

## 13. 风险与未决问题

**cold 档已在 2026-10-23 的实测中被否定并删除。** 原设计是对冻结的沙箱回收内存，让它留在内存里但不占 RAM。实测结论是这条路在并发下不成立，记录在此以免重复踩坑：

- **单沙箱成立。** 冻结后把 `memory.high` 压到 16 MiB，宿主机归还 71 MiB（释放 90%），耗时 7 ms，解冻 10 ms。
- **并发即失败。** 8 个沙箱下，批量下发、逐个串行、每批 3 个限流，**三种策略的达标率均为 0/8**，耗时都是 85 秒（超时跑满），释放量 136 到 230 MiB。同批沙箱走 snapshot 只要 1.1 秒、释放 413 MiB、成功 8/8。
- **原因不是 swap 容量。** 12 沙箱时 swap 还剩 1320 MiB，依然 11/12 超时。多 cgroup 同时触发 direct reclaim 会争抢同一套 LRU 和 swap 写入路径，谁都压不到目标。这是内核回收路径的固有行为，不是调参能绕过的。
- **顺带否定的几条。** `memory.reclaim` 只是请求，写完页还以 swap cache 形式留在内存，宿主机内存毫无释放（实测 MemAvailable 反而 −4.4 MiB）；`memory.zswap.writeback=0` 让压不动的页无处可去只能赖在内存；早期测到的 94% 降幅是合成数据（重复字节压缩 30 多倍）造成的假象，换成 numpy float64 后压缩比只有 2.8 到 3.6 倍。

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| snapshot 并发 restore 打磁盘带宽 | 实测 8 个并发 p50 从 84 ms 升到 465 ms | 升档限流；测 `runsc restore --background` 懒加载；快照放 NVMe |
| frozen 不释放内存 | 同时存活的沙箱数仍受 RAM 硬限 | 明确 frozen 只是短期档，空闲超过阈值必须落盘 |
| 长时间冻结后 guest 时钟和 RCU 告警 | 恢复后内核日志刷屏，极端情况 panic | 验证 Firecracker Paused 路径的 kvmclock 处理；超过阈值强制落盘 |
| checkpoint 落盘量随工作集线性增长 | 实测 50.2 MiB/沙箱，千个就是 50 GB | 按磁盘容量做准入；评估跨沙箱页去重 |
| gVisor 和 Firecracker 的内存记账不同 | gVisor 走 file/shmem，Firecracker 走 anon，结论不能互推 | Firecracker 后端的三档要重新实测一遍 |
| Python agent 的内存和启动成本 | hot 档压不到 10 MiB | v0.7 换 Rust agent |
| 删掉 live-fork 引起现有用户反对 | README 头条卖点没了 | 把卖点换成密度和重启不丢沙箱 |

**未决问题，要在讨论里定：**

- [ ] `runsc restore --background` 懒加载能把并发 restore 的 465 ms 压到多少。
- [ ] 跨沙箱的 checkpoint 页去重：同一模板的 N 个快照重复度很高，值不值得做内容寻址共享。
- [ ] frozen 到 snapshot 的默认阈值（5 min）要按真实负载的空闲分布重新定。
- [ ] 多租户模型放哪个版本：v0.6 仍是单 token，但 sqlite 表里预留 `owner` 列。
- [ ] Firecracker 后端的三档实测：需要一台有 /dev/kvm 的机器，目前三台服务器都没有。

## 14. 市场参照

现有产品都在两极：要么一直跑着靠 TTL 回收，要么空闲就快照落盘销毁。没有人对 microVM 做"冻结在内存加主动回收"的中间档，这是 v0.6 的差异化。以下为 2026-10-22 检索，数字除 DSec 外均为厂商自述。

| 产品 | 底层 | 空闲策略 | 公开数字 |
| --- | --- | --- | --- |
| [DeepSeek DSec](https://arxiv.org/html/2609.22978v1) | Firecracker | 一直跑，TTL 回收，RL 抢占时才快照杀掉 | 800 microVM/节点；约 90% 沙箱平均 CPU ≤ 5% 申请量；pmem+DAX 峰值内存降 40.2%，DAMON+FPR 时间积分内存降 21.2%（论文） |
| [AWS Lambda MicroVMs](https://docs.aws.amazon.com/lambda/latest/microvm-api/API_IdlePolicy.html) | Firecracker | IdlePolicy 自动 suspend 到快照再 terminate | 按秒计 vCPU/内存加快照存储 |
| [Blaxel](https://blaxel.ai/platform/sandboxes) | microVM | 15 s 无活动拍快照，standby 不计费 | 恢复约 25 ms |
| [PandaStack](https://www.pandastack.ai/blog/overcommit-microvm-density/) | Firecracker，核心开源 | 空闲即销毁，下次从快照恢复，uffd 按 4 MiB 块懒取 | 恢复 p50 179 ms |
| [Fly.io Sprites](https://fly.io/sprites/checkpoint-restore/) | Firecracker | 空闲休眠，块设备级 CoW checkpoint | 约 1 s |
| [Google Cloud Run gen1](https://docs.cloud.google.com/run/docs/configuring/cpu-allocation) | gVisor | 请求之间 CPU 节流到近零，实例留在内存 | 即 frozen 档的容器版量产先例 |
| [Unikraft Cloud](https://unikraft.com/blog/scale-to-zero) | unikernel | 初始化完拍快照再 scale-to-zero | 自称 < 10 ms，单机数千到 10 万 |

DSec 验证了超卖的统计前提（绝大多数沙箱绝大多数时间是空闲的），也给出了内存三板斧的实测，但它对暂停容器用了 `memory.reclaim`，对 microVM 仍是快照加杀进程。它的 pmem + DAX rootfs 值得在 v0.7 评估：guest 跳过自己的 page cache 直接共享宿主机那份，Cube 也这么做。

## 15. 多机架构：池

本节是设计，v0.6 不实现。写在这里是因为其中一部分（池的概念、快照的池归属）必须从一开始就进数据模型，否则 v0.7 加不进来。

### 15.1 池就是迁移域

一个池是一组**快照可以互相恢复**的节点。这不是设计出来的抽象，是被底层现实逼出来的边界：2026-10-23 的实测里，bm（EPYC 9K65 / 内核 7.0）的快照在 bj（Xeon 8255C / 内核 5.15）上连着失败两次，错误分别是：

```
runsc version does not match across checkpoint restore,
  checkpoint: release-20261012.0  current: release-20261019.0

incompatible FeatureSet: missing features:
  sha_ni avx512vbmi avx512_vbmi2 avx512_bitalg avx512_vpopcntdq avx512_bf16
  gfni vaes vpclmulqdq rdpid umip movdiri movdir64b xsaves fsrm
  cmp_legacy cr8_legacy sse4a misalignsse osvw topoext perfctr_core mmxext fxsr_opt
```

**指纹必须算，不能配。** 节点启动时自己计算：

```
fingerprint = hash(runsc 版本, CPU 特性集, 宿主机内核 release, guest 镜像版本)
```

人手配置一定会配错，而配错的后果是 restore 在运行时失败。Cube 的做法是在恢复时比对 `cpuid_hash` 和 `host_kernel_release`（`docs/guide/cross-node-snapshot.md`），属于**失败时才发现**；把指纹提到调度层，就变成**放置时就知道边界**，根本不会走到失败那步。

顺带一个 Cube 的缺陷值得避开：它的 `cpuid_hash` **只哈希第一个逻辑 CPU**，文档自己承认 big.LITTLE 或 Intel P+E 混合核机器可能哈希相同但实际不同。指纹要覆盖所有核心的特性交集。

**人为标签只能切细，不能合并。**

```
池 = (兼容性指纹, 可选标签)
```

指纹不同的节点永远不能同池，这是物理约束；指纹相同的节点可以用标签按租户、地域、成本再切开。这样运维有控制力，但配不出一个 restore 必然失败的池。

这个约束不是 gVisor 特有的。Firecracker 文档明文写着 *"Representing one CPU vendor as another CPU vendor is not supported"*，快照要求软硬件配置完全一致，它举的例子是同为 Intel 的 Ice Lake 快照也不能在 Cascade Lake 上恢复。T2A 和 T2CL 模板是为了让混合机群能跑同一批应用，不是为了让快照跨厂商迁移。所以**池是 checkpoint/restore 路线的固有代价，不是某家实现的缺陷**。

### 15.2 三层结构

[embedded content: 池架构 · 集群 / 池 / 节点三层，快照按池共享]

图里最关键的一条是**快照存储按池共享，不按节点**。它同时决定了两件事：池内任一节点都能恢复任一沙箱，以及节点挂掉时 parked 的沙箱不会跟着没。如果快照放在节点本地盘，沙箱就被钉死在那台机器上，多机退化成简单分片，失去调度弹性。

模板要**每池各烤一份**。模板本身就是个快照，它带着烤它那台机器的 CPU 特性。源（OCI 镜像加配置）是全局的，烤出来的产物是池内的。

### 15.3 只允许两个真相源

对 CubeSandbox 的审查里最严重的一条是状态散在五处（Cubelet 本地 bbolt、MySQL 35 张表、Redis、CubeMaster 内存补偿队列、CubeProxy 共享字典）且没有 reconcile。多机版本更容易犯这个错，所以先立规矩：

|  | 持有者 | 内容 | 谁是权威 |
| --- | --- | --- | --- |
| 期望状态 | PostgreSQL | 沙箱该存在、该在哪个池、该是什么档位 | 数据库 |
| 实际状态 | 节点本地 | 进程真的在不在、cgroup 真的是什么值 | 节点 |

reconciler 的工作就是比对这两者，差异就是工作项。**不允许出现第三个地方**，特别是不允许把主服务内存里的缓存当权威用。Cube 的 `live_vms` 全局锁和内存补偿队列就是这样坑的：补偿任务排在内存里，主服务重启就丢，留下孤儿 VM。

**数据库选 PostgreSQL 就够，不要 etcd。** 节点数量是几十到几百，不是几万，用不上 raft 那套复杂度。我们批过 Cube 单节点 18 个 systemd 单元，自己别重蹈覆辙。

**节点本地也存一份**（sqlite 或纯文件），但它存的是"我手上有什么"，不是期望。这一份是节点在主服务不可达时自治的依据，也是节点重启后重新接管的依据（第 5 节定的单机行为在多机下保持不变）。

### 15.4 从服务必须能独立活着

第 5 节已经定了单机的性质：控制器重启不杀沙箱，因为每个 VM 在自己的 systemd scope 里。多机版本要把这个性质保持住，否则主服务就成了单点故障。

| 故障 | 节点行为 | 丢失什么 |
| --- | --- | --- |
| 主服务挂掉 | 继续跑沙箱，继续响应已有沙箱的唤醒 | 只是不能创建新沙箱 |
| 节点重启 | 从本地状态恢复，按 API socket 重新接管，再向主服务汇报 | hot 沙箱丢，parked 不丢 |
| 网络分区 | 不自杀，继续服务，等网络恢复后对账 | 无 |
| 节点挂掉 | — | hot 沙箱丢；parked 沙箱因快照在池内共享而幸存 |

这就是 kubelet 的模型，apiserver 挂了 pod 照样跑。对比 Cube：它的控制器启动时 `kill_orphans` 会 SIGKILL 所有还活着的 Firecracker，注释自述分不清孤儿和正在管理的 VM（forkd 的 issue #301 是同一个毛病）。多机下这个错误的代价从"重启丢一台机器的沙箱"变成"一次升级丢全集群"。

**谁有权杀沙箱。** 只有 reconciler 在确认数据库里没有这条记录、且进程的 boot id 属于本机本次启动时，才能杀。其余情况一律不杀，宁可泄漏一个孤儿进程，也不要误杀用户正在用的沙箱。

### 15.5 通信：控制面和数据面分开走

这是本节最关键的设计决策。

|  | 路径 | 延迟要求 |
| --- | --- | --- |
| 控制面 | 节点 ↔ 主服务 | 容忍几十毫秒 |
| 数据面 | 网关 → 节点（直连） | 必须快，唤醒本身才 84 ms |

**唤醒请求绝不能经过主服务。** 网关从数据库或缓存里查到沙箱在哪个节点，直接打过去。Cube 的 auto-resume 要走"客户端 → CubeProxy → CLM → CubeMaster → Cubelet"四跳，这是它唤醒延迟高的原因之一。

**控制面用 gRPC 双向流，由节点主动拨出。**

- 节点拨出去，主服务不需要能连到节点：穿 NAT、省防火墙规则、主服务不用维护节点地址表
- 长连接双向，命令下发没有轮询延迟，容量上报用同一条连接
- 连接断开就是明确的失活信号，比心跳超时猜测准确
- 天然有背压

排除的方案：纯轮询（命令下发有延迟，空闲时浪费）；主服务 RPC 调节点（要求节点可达，NAT 后面就废了）；消息队列（多一个组件要运维，Cube 把 Redis 当锁管理器加事件总线，Redis 一重启整个集群失忆）；拿数据库当总线（节点轮询数据库，规模一大就是热点）。

**流上传什么。** 节点上报**实测值**，不上报配额：`memory.current`、PSI、实际沙箱列表及其档位、指纹、磁盘余量。Cube 的 issue #1939 和 #1962 就是按配额而非实测做准入，超卖时水位保护失效。主服务下发的是期望档位和放置决策，不下发具体指令，具体怎么做由节点自己决定。

### 15.6 跨池降级为冷启动，升级靠多版本共存

**跨池不搬状态。** 用户想把沙箱从池 A 挪到池 B，系统不报错，而是从模板重新起一个。用户看到的是慢了几百毫秒，而不是迁移失败。代价是沙箱内的运行时状态丢失，这一点必须在 API 语义上写明，不能静默地换一个沙箱给用户。

这比 Cube 好在：Cube 是在 restore 时比对 `cpuid_hash`，不匹配就失败；我们在**放置时**就知道边界。

**升级是池模型最难的运维问题。** 实测到 runsc 差一周的 release 就拒绝恢复，这意味着升级会作废池内所有已落盘的快照。三种应对，按推荐度排序：

1. **多版本共存**，学 Cube。节点上同时保留多个版本的 runsc，快照记录自己用哪个版本烤的，恢复时用对应版本。Cube 的组件多版本仓库（`docs/guide/component-multiversion.md`）就是干这个的，这是它在这个问题上比 gVisor 生态做得好的地方。
2. **滴答式池**。升级时新建一个新指纹的池，新沙箱进新池，老池不再接新活并自然排空。不需要多版本共存，代价是短期内要多一倍机器。
3. **停机升级**。所有 parked 沙箱先唤醒再重新落盘，或者直接丢弃。只适合开发环境。

这也是为什么指纹要把 runsc 版本算进去：升级一台节点，它的指纹就变了，自动脱离原池，不会造成一个混着两个版本、restore 时才报错的池。

### 15.7 分阶段

| 阶段 | 内容 | 验收条件 |
| --- | --- | --- |
| 1 | 单节点三档 | 已有实测数据（第 4 节） |
| 2 | 单池多节点，快照存储池内共享 | 同一沙箱在池内两节点间迁移成功，token、校验和、计数器全对得上 |
| 3 | 多池，集群调度器 | 跨池创建走冷启动不报错；指纹不同的节点自动分开 |
| 4 | 懒加载快照 | 恢复延迟不随快照大小线性增长 |

**阶段 2 现在就能做**，需要两台指纹相同的机器。目前三台服务器里 bj 和 hk 都是腾讯云标准云主机、都是 5.15 内核，CPU 很可能同型号，装上同版本 runsc 就是一个池。

这个验证必须先做，因为目前只验证了**异构不可迁移**这个负面结论，还没验证**同池可迁移**这个正面假设。如果同池迁移也有坑（比如网络状态、文件描述符、时钟），整个池模型要重新想。

**还没想清楚的：**

- [ ] 快照存储用什么：NFS、MinIO、还是节点间按需传输。影响恢复延迟和节点故障的爆炸半径。
- [ ] 同池内节点间迁移时，沙箱的 IP 和端口映射怎么跟着走。
- [ ] 调度器选池的策略：容量、地域、成本怎么加权。
- [ ] 池内节点数的上限：快照存储的带宽会不会成为瓶颈。

### 7.2 小文件是硬约束，不是调优问题

| 操作 | 本地盘 | JuiceFS（PG） | JuiceFS（Redis） |
| --- | --- | --- | --- |
| 创建小文件，单线程 | 0.03 ms | 6.66 ms | 4.76 ms |
| 创建小文件，并发 32 | 0.05 ms | 3.20 ms | **1.89 ms** |
| 解压 3000 个文件 | 42 ms | 24.3 s | 15.8 s |
| 顺序写 64 MiB | 554 ms | 636 ms | — |

三个结论：

- **大文件几乎无损**（636 对 554 ms）。数据文件、模型输出、归档包放卷里完全没问题。
- **小文件慢 60 到 380 倍**，而且并发到 8 就封顶，`--writeback` 几乎无效（3.09 降到 2.91）。
- **换 Redis 元数据引擎没意义。** 快 40% 但仍差两个数量级。决定性证据：Redis 服务端执行 1000 次操作只要 6 ms（0.006 ms/次），而 JuiceFS 创建一个文件要 1.89 ms，**是元数据引擎本身耗时的 300 倍**。瓶颈在 FUSE 上下文切换和客户端每文件的多次元数据操作，不在引擎。所以继续用 PG，不为性能引入 Redis 的持久化风险。

这是"对象存储 + 网络元数据 + FUSE"这类架构的固有特性，换 NFS、Ceph 都不会有数量级改善。因此：

```
/workspace              ← JuiceFS 卷：源码、配置、数据、输出
/workspace/node_modules ← bind mount 本地目录盖住
/workspace/.venv        ← 同上
构建缓存、__pycache__  ← 同上
```

### 7.3 依赖和缓存：四层

本地层跑得快但会丢，卷耐久但小文件慢。四层分别解决一部分：

1. **预装进模板。** 常用依赖烤进只读 base，走本地盘、零成本、多沙箱共享一份。覆盖大部分情况。
2. **本地层在 park 和 resume 之间天然保留。** `runsc checkpoint` 只打包内存，per-child 的可写 rootfs 是磁盘上独立文件，hot 到 frozen 到 snapshot 全程不动。**日常使用根本不触发归档。**
3. **只在换机器或驱逐时打包。** tar+zstd 成一个文件存进卷。卷怕小文件数量，不怕单个大文件。
4. **共享包缓存。** 只读的 pip/npm 缓存挂进所有沙箱，重装时不走网络。

**归档实测（3 万文件 / 923 MiB，按真实 `node_modules` 分布生成）：**

| 方式 | 打包 | 恢复 | 往返 | 体积 |
| --- | --- | --- | --- | --- |
| 逐文件走卷 | — | — | \~57 s | — |
| 纯 tar | 15.9 s | 18.6 s | 34.5 s | 887 MiB |
| zstd -1 | 8.9 s | 11.1 s | 20.0 s | 261 MiB |
| **zstd -3** | **4.8 s** | **11.4 s** | **16.1 s** | **252 MiB** |

两个反直觉的点：**读本地 3 万个文件只要 0.1 秒**，时间全在写卷；**zstd -3 比 -1 还快**，因为瓶颈是卷的写入带宽，压得越小写得越快。所以压缩是必须的，默认 zstd -3 多核。

**待做的优化：只归档相对模板的增量。** 模板里已有的不存，只存用户额外装的。增量通常只有几 MB，往返不到 1 秒。

### 7.4 密度账：reflink 让磁盘消耗等于写入量

实测：克隆 10 份 1 GiB 的模板 rootfs，**零磁盘消耗**；之后每个写入不同量，真实占用严格等于写入量，一字节不多（写 2 MiB 占 2 MiB，写 200 MiB 占 200 MiB）。10 个沙箱逻辑上 10 GiB，实际只用 777 MiB，省 13.2 倍。

所以单机能 park 多少个沙箱，**线性取决于用户偏离模板多远**：

| 用户行为 | 每沙箱增量 | 加 50 MiB 快照 | 100 GB 能装 |
| --- | --- | --- | --- |
| 只改源码 | 5 MiB | 55 MiB | **1860 个** |
| 装几个小包 | 50 MiB | 100 MiB | 1020 个 |
| 混合（实测均值） | 77 MiB | 127 MiB | 806 个 |
| 装一整套新栈 | 900 MiB | 950 MiB | 107 个 |

**推论：模板质量直接决定架构性能。** 模板预装得越全，增量越小，磁盘装得下越多，回收越少被触发，7.3 那条 11 秒的恢复路径几乎不走。反之模板很薄，回收就从例外变成常态。

所以要让"把当前沙箱固化成新模板"这个操作很容易（forkd 现有 `persist: true` 导出 base 的能力）。它不只是方便，是**维持密度的必要机制**。

**归档什么时候触发：**

| 场景 | 频率 | 用户是否在等 |
| --- | --- | --- |
| 节点宕机 | 罕见 | 是 |
| 计划内腾空节点 | 偶尔 | 否，可提前后台搬 |
| 本地磁盘吃紧回收 | **由上表决定** | 是 |
| 长期不用被 GC | 策略决定 | 是 |

### 7.5 部署硬性要求

- **节点的 rootfs 所在文件系统必须是 XFS（`reflink=1`）或 btrfs。** bm 的根分区是 ext4，实测时是临时建 loopback XFS 才做成的。没有 reflink，7.4 的密度账全部作废，每个沙箱要拷一份完整 rootfs。
- **启动时检查并拒绝启动，不允许静默退化成全拷贝。** forkd 的 `chain::reflink_copy` 就是这样退化的，而且 FICLONE 号还写错过（#322），结果在 XFS 和 btrfs 上也一直在做全量流式拷贝。
- **卷用托管对象存储，元数据用主库的 PostgreSQL。** 不单独维护 Redis。元数据丢了等于全盘数据丢失，跟着主库的备份和高可用走是最省事的做法。
- **对象存储端选 RustFS 而不是 MinIO。** MinIO 社区版已进入维护模式，控制台和复制移到商业版 AIStor，官方二进制下载地址已失效（实测 410）。RustFS 是 Apache 2.0，Rust 写的，和本项目技术栈一致。

## 16. 重构：从 v0.5 的 controller 到主从架构

这一节是 v0.6 的实施计划。前面几节定了做什么，这节定怎么落地。

### 16.1 重写而不改造

现有 `forkd-controller` 9270 行，成分很偏：

| 模块 | 行数 | 处置 |
| --- | --- | --- |
| `http.rs` | 5028 | **删**。单文件 5000 行，同步阻塞的 REST，没有作业模型 |
| `state.rs` | 2158 | **删**。内存 map 刷 JSON，重启 `kill_orphans` 杀光沙箱 |
| `netns.rs` | 418 | **重写**。预建池改动态创建，netlink 调用可参考 |
| `api.rs` | 553 | 部分留。请求响应类型可以改 |
| `auth.rs` | 204 | **留**。常量时间比较做对了 |
| `audit.rs` | 265 | **留**。SIGHUP 轮转做对了 |

要删的占 78%，而且是最核心的两个。改造要先拆掉 `live_vms` 全局锁、内存补偿队列、单机假设，拆完剩下的不比重写多。所以重写，把 `auth.rs` 和 `audit.rs` 搬过去。

### 16.2 一个二进制，两种模式

```
forkd serve --master              # 只跑主：REST API、调度、PostgreSQL
forkd serve --node                # 只跑从：管 runsc、本地 sqlite、拨 gRPC
forkd serve --master --node       # 单机：两个都跑在一个进程里
```

单机时两个模块在同一进程内，**但仍走同一套 gRPC 接口**（进程内走 in-memory channel）。这样单机和多机的代码路径完全一致，不会出现"单机能跑多机不行"的情况。forkd v0.5 的 CLI 有一套绕过 daemon 直连 Firecracker 的路径，README 自己承认两条路不兼容，这是要避开的反例。

**进程形态。** 一个沙箱是一条 `systemd scope → jailer/runsc → guest`，**不挂在 forkd 进程下**。实测过：`kill -9` 主进程后 20/20 沙箱存活、token 不变，重新接管 134 ms。

### 16.3 加一台机器

目标是一条命令，池的归属自动算出来，人不参与（第 15.1 节：指纹必须算，不能配）。

```bash
# 1. 在主服务上签一个一次性注册令牌（默认 15 分钟过期）
forkd node-token create --ttl 15m
# → nt_7f3a9c...

# 2. 在新机器上
forkd serve --node \
    --master grpcs://master.internal:7443 \
    --token nt_7f3a9c...
```

节点启动后做四件事：

1. **自检**。没有 XFS/btrfs、没有 cgroup v2、runsc 不在，直接拒绝启动并说明原因（第 7.5 节）。
2. **算指纹**。`hash(runsc 版本, CPU 特性集, 内核 release, guest 镜像版本)`。
3. **拨连主服务**。用注册令牌换一对长期客户端证书（第 16.5 节），令牌随即作废。
4. **归池**。主服务按指纹找现有池，没有就建一个新的。运维看到的是节点自己跑进了某个池。

**节点下线**是反过来的：`forkd node drain <id>` 把上面所有沙箱降到 detached（实测 1.1 秒/个），然后调度器在同池其它节点上拉起来。没有同池节点就停在 detached，等新机器。

### 16.4 模板可配

模板是一个 YAML 加一个镜像引用，不再写死 python+numpy：

```yaml
name: py-ml
from: docker.io/library/python:3.12-slim
setup:                      # 烤进只读 base，所有沙箱共享一份
  - pip install numpy pandas scikit-learn
serve: python -m myagent    # 模板快照在它就绪后拍
resources:
  memory: 512Mi             # memory.high；memory.max 为其 1.5 倍
  cpu_weight: 100
idle:
  to_frozen: 30s
  to_snapshot: 5m
  to_detached: 1h
```

**三个要点：**

- **依赖必须进 `setup`，不要让用户进沙箱装。** 第 7.4 节算过：模板预装得越全，每沙箱增量越小，单机装得下的沙箱从 107 个升到 1860 个。
- **每池各烤一份**（第 15.2 节）。源是全局的，烤出来的快照带着那台机器的 CPU 特性，只在本池有效。新池第一次用某模板时自动烤。
- **沙箱可以固化成新模板**：`forkd template from-sandbox <id> --name py-ml-v2`。这不是方便功能，是维持密度的必要机制。

### 16.5 密钥：两层，解决两个不同的问题

**第一层：主从认证。** 不能随便一台机器连上来就能接活，否则攻击者假装成节点就能拿到用户的沙箱。

- **注册是一次性的**：`forkd node-token create --ttl 15m` 签一个短命令牌，节点用它换一对客户端证书，令牌立即作废。令牌泄漏的窗口只有 15 分钟且只能用一次。
- **日常连接走双向 mTLS**：节点拿证书拨出，主服务验证。节点不需要开端口（第 15.5 节），所以它在 NAT 后面也能工作。
- **摧销**：`forkd node revoke <id>`，证书进吐鲁列表，连接立即断。

**第二层：沙箱访问凭证。** 这是修 CubeSandbox 和 forkd 现在都有的漏洞：**知道沙箱 ID 就能连**。

- CubeSandbox 把 E2B 的 `envd_access_token` 硬编码成 `None`（`CubeAPI/src/services/sandboxes.rs:328`），`secure` 字段解析了不用；forkd 的 traffic token 是 opt-in，老沙箱默认开放。
- v0.6 的做法：**每个沙箱创建时必定生成一个 128 位随机 token**，访问它的任何端口都要带。没有"默认公开"这个模式，也不允许关掉。
- token 存在数据库，网关校验。沙箱 ID 改为 128 位随机，不再是可猜的计数器。

**两个方向都要验。** 节点用客户端证书证明自己是合法节点；主服务用服务端证书证明自己是真主服务，否则攻击者可以假装成主服务骗节点交出沙箱或执行恶意指令。

**连接方向本身就是一道防线。** 节点主动拨出，主服务从不去连节点，只能在节点已建立的流上回消息。所以节点不用开任何入站端口，主服务被攻破也扫描不到、连不上未注册的机器。

**命令不单独签名。** mTLS 已经保证了端到端身份，再给每条指令加签是重复工作，且让协议复杂很多。这是有意的取舍，写在这里以免后来被当成疏漏。

**加机器这个动作，面板产出的是一行命令，而不是一台机器。** 主服务连不上一台还没联系过它的机器：没地址、没凭证、对方也没有 forkd 在监听。所以面板负责生成 token、选池子、给出可复制的安装命令，之后展示这台机器的状态；那一行命令必须在新机器上跑一次。接口是 `POST /v1/node-bootstrap`。k3s、Tailscale、Nomad client、GitHub Actions runner 都是这个形状，原因相同。

准确地说，主服务连不上的是一台**什么都没在跑**的机器。实测 bm（2026-10-23）：`tat_agent` active，`cloud-init` 24.4 active（configdrive）。所以在这片机器上，面板单击加机器是做得到的，只是「机器上已经有东西在跑」这一半由镜像或厂商 agent 承担，而不是人敲命令。

四条路，按凭证被盗的爆炸半径排：

| 谁调 join | 机器上要预先有 | 主服务要持有 | 凭证被偷的后果 |
| --- | --- | --- | --- |
| 自助加入 + 面板审批 | 镜像里有 forkd | 什么都不用 | — |
| cloud-init / user-data | 镜像里有 forkd | 创建实例权限 | 烧钱，碰不到已有机器 |
| TAT / SSM 下发 | 厂商 agent | 执行命令权限 | 账号内任意实例 root |
| 人粘贴命令 | 什么都不用 | 什么都不用 | — |
| SSH（不采用） | sshd + 入站端口 | 每台机器私钥 | 名单上机器 root |

**采用第一条。** 它比注册 token 方案更强：没有任何秘密需要送到机器上。机器开机后 forkd 自己拨出，报指纹和实例 id，主服务记为 pending；审批通过才发证书。审批依据是用**只读**的云 API 问一句「这个实例 id 是不是我账号里的」——所有方案里最窄的权限；没有云 API 则退回人工比对指纹。

**这不是另一套认证模型。** 四条路最后都调同一个 join 接口，差别只在谁去调它。要加的是 pending/审批这一层和一个 provider 适配，mTLS 那套不动。

两层是正交的：第一层管"谁能当节点"，第二层管"谁能进这个沙箱"。多租户（谁能看到哪些沙箱）是第三件事，v0.6 仍是单管理员 token，但数据库表里预留 `owner` 列。

### 16.6 第一个版本的范围

**做到单机能看能控，架构是多机的。**

| 做 | 不做 |
| --- | --- |
| 一个二进制两种模式，单机 `--master --node` | 多节点实际部署 |
| 四档升降，空闲和 PSI 双触发 | 跨节点调度算法 |
| PostgreSQL 期望态 + 节点 sqlite 实际态 + reconcile | 多池管理 |
| 重启重新接管（已测：20/20，134 ms） | 多租户 |
| 模板 YAML 和构建 | Web UI |
| 两层密钥 | 懒加载快照 |
| `forkd top` 终端视图 |  |

**`forkd top`** 是第一个能看的东西，几百行：实时列出所有沙箱的档位、`memory.current`、CPU、所在节点，支持手动升降档。Web UI 等核心跑顺了再做，因为它是另一个量级的工作量。

**验收：** 在 bm 上 `forkd serve --master --node` 跑起来，用 `forkd top` 看到沙箱自动从 hot 降到 frozen 再到 snapshot，请求到达时自动升回 hot，`kill -9` 主进程后沙箱全活着且重启后能接管。
