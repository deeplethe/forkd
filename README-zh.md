<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="Sandstorm" src="docs/logo.svg" width="383">
</picture>

</div>

<br/>

<div align="center">

[工作原理](#工作原理) · [实测数据](#实测数据) · [快速开始](#快速开始) · [状态](#状态)

[![CI](https://img.shields.io/github/actions/workflow/status/deeplethe/sandstorm/ci.yml?branch=dev&style=flat-square&label=CI&labelColor=161B22)](https://github.com/deeplethe/sandstorm/actions)
[![License](https://img.shields.io/badge/LICENSE-APACHE%202.0-3FB950?style=flat-square&labelColor=161B22)](LICENSE)
[![Rust](https://img.shields.io/badge/BUILT%20WITH-RUST-F74C00?style=flat-square&labelColor=161B22&logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org)
[![DeepLethe](https://img.shields.io/badge/BUILT%20BY-DEEPLETHE-F1F2F5?style=flat-square&labelColor=161B22)](https://deeplethe.com)
[![gVisor](https://img.shields.io/badge/ISOLATION-GVISOR-2D333B?style=flat-square&labelColor=161B22)](https://gvisor.dev)
[![English](https://img.shields.io/badge/LANG-ENGLISH-2D333B?style=flat-square&labelColor=161B22)](README.md)

</div>

**一个有意超卖的沙箱集群。** agent 的沙箱大部分时间都闲着。Sandstorm 先把闲置的沙箱暂停，再把它 checkpoint 到磁盘；请求一到，马上恢复。所以一台机器能装下的沙箱，远多于它能同时跑着的数量。每个沙箱都是一个 [gVisor](https://gvisor.dev) 容器，有自己的网络命名空间。Sandstorm 跑在普通云主机上，不需要 KVM，也不需要嵌套虚拟化。

六台 2 核 2 GB 的云主机，全部常驻只能放 **286** 个 Python HTTP 服务；用 Sandstorm 放了 **20,000** 个。之后随机抽 1,200 个去请求，1,198 个都正常响应。

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-overview-dark.svg">
  <img alt="六台 2 核 2 GB 机器，全部常驻只能放 286 个，Sandstorm 放了 20,000 个沙箱；左边是容量，右边是延迟。左下按堆大小看每个节点作为沙箱和作为普通进程能常驻的数量，每种大小下面标着从磁盘唤醒的时间；右边是按闲置时长看请求延迟，以及流量加大时的延迟和拒绝比例（对照 round 3）" src="docs/assets/charts/fleet-overview-light.svg" width="800">
</picture>

</div>

master 在 `/` 提供控制台：每个沙箱在哪一档、在各档之间流动多快、每个节点还剩多少内存。下图是模拟的 6 节点集群。

<p align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/console-dark.webp">
  <img alt="Sandstorm 控制台：顶部是沙箱数、常驻数、节点数、每分钟创建数和 API p99；左边是常驻、冻结、磁盘、离线四档之间的流动；右边是各档沙箱数随时间变化、生命周期速率、节点余量和延迟。" src="docs/assets/console-light.webp">
</picture>
</p>

## 工作原理

每个沙箱处在四个档位之一。闲得越久降得越低，每次降一档；任何请求都让它直接回到 hot。

| 档位 | 状态 | 占内存 | 回到 hot |
|---|---|---|---|
| **hot** | 运行中 | 是 | — |
| **frozen** | 原地暂停（`cgroup.freeze`），冷页被压进 zswap | 部分 | 约 10 ms |
| **snapshot** | checkpoint 到节点本地盘，释放网络命名空间 | 否 | 约 0.2 s |
| **detached** | 只剩它的卷 | 否 | 冷启动 |

- **一个 master，多个节点。** master 把期望状态存在 SQLite 里，决定沙箱放在哪台机器。可以由 master 凭地址和配对码连上节点，也可以由 NAT 后面的节点主动连 master。
- **流量唤醒。** 请求先到沙箱所在节点上的网关，网关把沙箱唤醒，再转发请求。
- **节点会保护自己。** 内存不够时，节点会拒绝新任务，不会把自己撑爆，ssh 也一直能连上。
- **只有一个二进制。** `sandstormd` 既是 master，也是节点，还带 web 面板。

## 延迟和开销

开头总览图里另外三张图，先是右边两张：

- **唤醒。** 在节点本机上测，不经过网络。热的沙箱 3.8 ms 就有响应。冻结的沙箱，解冻只要 11 ms，其余时间主要花在把堆内存从 zswap 读回来，合计约 100 ms。已落盘的沙箱，64 MiB 的堆恢复约 230 ms。
- **有流量的时候。** 每个节点放 40 个沙箱，只装得下约 12 个同时醒着，少数几个沙箱拿走大部分请求。唤醒时内存不够，节点会先把最少被用到的沙箱落盘腾出位置；2 秒内起不来的唤醒直接拒绝，不排队。所以每个节点每秒 1、2、4、8 个请求时，被拒绝的只有 **1.1%、0.1%、0.2%、2.5%**，修复前是 13%、26%、32%、33%（三遍，共 40,500 个请求）。代价在长尾：要等恢复的请求，p99 在各个速率下都是 2 秒左右。超过节点的周转能力时——每节点 80 个、每秒 8 个请求——会拒绝 10.6%，p99 是 3.2 秒。
- **更重的沙箱。** 堆内存大的时候，一个沙箱的开销和同一个程序直接作为普通进程跑差不多：64 MiB 时每个节点 11 个对 10 个，256 MiB 时 4 个对 3 个，512 MiB 和 1 GiB 时两边都是 2 个和 1 个，因为节点会把闲置的页压进 zswap，而测试用的堆特别好压缩。堆小的时候，gVisor 自身的开销占大头，16 MiB 时是 19 个对 24 个，分档正是用来弥补这个差距的。从磁盘恢复的时间随堆增长：64 MiB 要 0.2 s，256 和 512 MiB 约 1 s，1 GiB 要 3.6 s。

## 同样的机器，不用 Sandstorm

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-compare-dark.svg">
  <img alt="一台 2 核 2 GB 节点能放的 Python HTTP 服务：普通进程约 96 个，bubblewrap 94 个，Podman 88 个，Docker 60 个，Docker + gVisor 32 个，Sandstorm 全部常驻 48 个，Sandstorm 闲置的分档落盘 3,333 个" src="docs/assets/charts/fleet-compare-light.svg" width="800">
</picture>

</div>

同一个 Python HTTP 服务，同样六个节点，每种运行时都全部放在内存里，一直加到只剩 512 MiB，每个节点测两遍（[`baselines.py`](bench/fleet/baselines.py)）。同样是 gVisor 隔离，Sandstorm 全部常驻时比 Docker + gVisor 多放一半，因为它没有守护进程，也不需要每个容器一个 shim；闲置的分档落盘之后，比任何全部常驻的方式都多出两个数量级。

| 每节点，2 vCPU，1.9 GB | HTTP 服务常驻数 | 64 MiB 堆 |
|---|---:|---:|
| 普通进程 | 96 | 11 |
| bubblewrap | 94 | 11 |
| Podman (crun) | 88 | 12 |
| Docker (runc) | 60 | 10 |
| Docker + gVisor | 32 | 9 |
| **Sandstorm，全部常驻** | **48** | **13** |
| **Sandstorm，闲置的分档** | **3,333** | |

启动速度在同一台机器上测（bm：AMD EPYC 9K65，2 vCPU，3.6 GB；dev 872682b），方法和基线一样：一次启动一个，每 50 ms 请求一次，直到它响应（[`starttime.py`](bench/fleet/starttime.py)）。Sandstorm 的时间包括 master 调度和节点建网络。

| 启动到首次响应，p50 | web | 64 MiB 应用 |
|---|---:|---:|
| 普通进程 | 52 ms | 103 ms |
| bubblewrap | 52 ms | 103 ms |
| Docker (runc) | 192 ms | 244 ms |
| Docker + gVisor | 217 ms | 333 ms |
| **Sandstorm** | **167 ms** | **289 ms** |
| **Sandstorm，预热模板** | **159 ms** | **168 ms** |

Sandstorm 启动 64 MiB 应用的 289 ms 里，master 和建网络 37 ms，准备 bundle 5 ms，runsc 55 ms，剩下 189 ms 是 Python 导入 numpy、填充堆。预热模板直接恢复一个已经启动好的应用镜像，省掉了最后这段；它需要手动开启，因为每个沙箱都从同一份内存开始，模板要说明怎么重新播种随机数。50 ms 的轮询间隔是下限。

Podman 下的 CRIU checkpoint/restore 是标准工具里最接近落盘档位的做法：64 MiB 应用落盘 0.3 s，恢复到首次响应 0.24 s；Sandstorm 恢复同一个应用要 0.2 s。E2B、CubeSandbox、microsandbox 都基于 Firecracker，需要 KVM，这几台云主机没有；它们在下面一台有 KVM 的机器上测了，同类项目的对比见 [docs/COMPARISON.md](docs/COMPARISON.md)。

## 在有 KVM 的机器上，对比其他沙箱

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/baremetal-start-dark.svg">
  <img alt="同一台 KVM 机器上 64 MiB 应用的启动到首次响应时间和单实例内存：Sandstorm Firecracker 预热模板 74 ms、10 MiB，CubeSandbox 78 ms、12 MiB，Sandstorm gVisor 预热模板 123 ms、97 MiB，Sandstorm gVisor 545 ms、98 MiB，Docker 414 ms、88 MiB，Docker + gVisor 660 ms、107 MiB，microsandbox 1.1 s、166 MiB，Kata 3.4 s、310 MiB" src="docs/assets/charts/baremetal-start-light.svg" width="800">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/baremetal-clone-dark.svg">
  <img alt="把运行中的 64 MiB 应用克隆成 1、10、20 份并全部响应：Sandstorm Firecracker 137、528、980 ms；CubeSandbox 542、973、1,046 ms；microsandbox 658、835、1,152 ms（到运行）；Sandstorm gVisor 458、1,340、2,280 ms" src="docs/assets/charts/baremetal-clone-light.svg" width="800">
</picture>

</div>

需要 KVM 的运行时，放在一台有 KVM 的机器上测：2 × Xeon E5-2637 v2、31 GB、Ubuntu 24.04。各运行时轮流跑，同样两个程序、同一个测试脚本（[`compare.py`](bench/baremetal/compare.py)）。在这里 Sandstorm 也可以把每个沙箱跑成 [Firecracker](https://firecracker-microvm.github.io) 微虚拟机（`--backend firecracker`）；用预热模板时，它是所有被测项目里启动最快的，内存和 CubeSandbox 相当。

| p50 | web | 64 MiB 应用 | 内存，web / 64 MiB 应用 |
|---|---:|---:|---:|
| **Sandstorm，Firecracker，预热模板** | **62 ms** | **74 ms** | 9.9 / **10.5 MiB** |
| CubeSandbox 0.7.3 | 72 ms | 78 ms | **8.9** / 12.2 MiB |
| **Sandstorm，gVisor，预热模板** | 107 ms | 123 ms | 23.3 / 97.3 MiB |
| **Sandstorm，gVisor** | 234 ms | 545 ms | 24.1 / 97.9 MiB |
| Docker + gVisor | 327 ms | 660 ms | 30.8 / 106.8 MiB |
| microsandbox 0.7.8 | 1,117 ms | 1,119 ms | 80.2 / 166.2 MiB |
| Kata 4.2（QEMU） | 3,005 ms | 3,397 ms | 223 / 311 MiB |

Sandstorm 在 Firecracker 上给运行中的沙箱打快照要 27 ms，CubeSandbox 要 252 ms；克隆 n 份就是这一次快照加一次 fork 调用。子沙箱在节点上逐个创建，每个约 50 ms，而 CubeSandbox 和 microsandbox 是同时创建，所以 n 更大时差距会缩小。E2B 自托管没有搭。方法、原始数据和测试中发现的问题见 [bench/baremetal/results/2026-10-30](bench/baremetal/results/2026-10-30/README.md)。

## 实测数据

六个节点（AMD EPYC，每台 2 vCPU、1.9 GB），一个 master（2 vCPU），runsc `release-20261019.0`，2026-10-28。

| | |
|---|---|
| 以每秒 20 个的速度创建 20,000 个 Python HTTP 服务 | 两遍都是 20,000 个全部起来，没有人工重试 |
| 每个节点随机抽 100 个去请求 | 1,200 个里 1,198 个响应；p50 1.3 s，p99 2.8–5.5 s |
| 唤醒：冻结的 / 从磁盘，64 MiB 应用 | 11 ms / 200 ms（Sandstorm 自身耗时） |
| 一个节点同时唤醒 20 / 40 个 | 全部醒来；最慢 6 s / 13 s |
| 64 MiB 应用从创建到首次响应 | 一个节点同时启动 5 个、每 0.25 s 查一次时 p50 1.4 s；一次一个时 0.29 s（见上文） |

几点说明：为了让一轮测试在几分钟内跑完，闲置计时被调短了。测试用的堆是 `numpy.ones`，在 zswap 里压缩率异常高。20,000 是这次测试停下来的地方，不是上限。方法、原始数据、每一遍运行，以及测试中发现的 bug，都在 [bench/fleet/results/2026-10-28](bench/fleet/results/2026-10-28/README.md)；上一轮见 [2026-10-26](bench/fleet/results/2026-10-26/README.md)。

## 快速开始

需要 Linux 和 cgroup v2，`PATH` 里要有 [`runsc`](https://gvisor.dev/docs/user_guide/install/)，编译需要 Rust。

```bash
git clone https://github.com/deeplethe/sandstorm.git && cd sandstorm
cargo build --release -p sandstorm-core
sudo ./target/release/sandstormd serve --master --node
```

这样会在一个进程里同时跑 master 和节点，面板在 `https://127.0.0.1:8080`，API token 在 `/var/lib/sandstorm/pki/api-token`。沙箱从模板启动，模板是一个 rootfs 加一份 OCI spec，用 `POST /v1/templates` 注册；最小的例子见 `scripts/fleet.sh seed`。

**多台能 ssh 上去的机器组成集群：**

```bash
scripts/fleet.sh up --master m1 --nodes n1,n2,n3 --listen 80
scripts/fleet.sh seed     # 在每个节点放一个模板
scripts/fleet.sh verify   # 启动一个沙箱，确认真的能跑
```

**已经装好的节点**，用 `SANDSTORM_LISTEN=0.0.0.0:80` 监听。第一次启动时它会把配对码写到 `/var/lib/sandstorm/pki/node/pairing-code`。把地址和配对码交给 master，CA 和 API token 用 master 上 `/var/lib/sandstorm/pki` 里的那两个文件：

```bash
curl --cacert ca.pem -H "Authorization: Bearer $(cat api-token)" -H 'Content-Type: application/json' \
  https://master:8080/v1/nodes -d '{"address": "10.0.0.5", "pairing_code": "<配对码>"}'
```

配对码重启后不变，拿到它就能在那个节点上执行任何操作，请像 root 密码一样保管。

HTTP API 见 [docs/API.md](docs/API.md)。集群压测见 [bench/fleet](bench/fleet/README.md)。

## 状态

v0.6 还没有发布，改动很大：后端从 Firecracker 微虚拟机换成 gVisor，从单机变成集群。最近一次发布是 v0.5.3，那是基于 Firecracker 的 fork 运行时，它的 README 见[那个 tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3)。架构决策记录在 [docs/adr](docs/adr/README.md)（英文）。

接下来，来自集群实测发现的问题：

- [x] 唤醒时腾出位置：把最少被用到的沙箱落盘，而不是拒绝请求。
- [x] 节点暂缓的创建，在内存空出来后重试，而不是停放；跑起来后清除它的错误状态。
- [x] 清理死掉的沙箱，连同它的网络；如果还需要它，就重新启动。
- [ ] master 在分配时预留容量，避免一波突发请求超过节点上次上报的余量。
- [ ] 同一节点上并行恢复多个 snapshot。

## 许可证

[Apache-2.0](LICENSE)
