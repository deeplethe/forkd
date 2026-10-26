<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="forkd" src="docs/logo.svg" width="220">
</picture>

</div>

<br/>

<div align="center">

[工作原理](#工作原理) · [实测数据](#实测数据) · [快速开始](#快速开始) · [状态](#状态)

[![CI](https://img.shields.io/github/actions/workflow/status/deeplethe/forkd/ci.yml?branch=dev&style=flat-square&label=CI&labelColor=161B22)](https://github.com/deeplethe/forkd/actions)
[![License](https://img.shields.io/badge/LICENSE-APACHE%202.0-3FB950?style=flat-square&labelColor=161B22)](LICENSE)
[![Rust](https://img.shields.io/badge/BUILT%20WITH-RUST-F74C00?style=flat-square&labelColor=161B22&logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org)
[![gVisor](https://img.shields.io/badge/ISOLATION-GVISOR-2D333B?style=flat-square&labelColor=161B22)](https://gvisor.dev)
[![English](https://img.shields.io/badge/LANG-ENGLISH-2D333B?style=flat-square&labelColor=161B22)](README.md)

</div>

**一个有意超卖的沙箱集群。** agent 的沙箱大部分时间都闲着。forkd 先把闲置的沙箱暂停，再把它 checkpoint 到磁盘；请求一到，马上恢复。所以一台机器能装下的沙箱，远多于它能同时跑着的数量。每个沙箱都是一个 [gVisor](https://gvisor.dev) 容器，有自己的网络命名空间。

六台 2 GB 的云主机，全部常驻只能放 **431** 个沙箱，用 forkd 跑到了 **7,284** 个。

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-per-node-dark.svg">
  <img alt="每台机器的沙箱数：全部常驻时每台约 72 个，分档后每台 1,162 到 1,318 个" src="docs/assets/charts/fleet-per-node-light.svg" width="800">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-tiers-dark.svg">
  <img alt="以每秒 15 个的速度创建时的档位分布：几乎所有沙箱都在 snapshot 档，运行中的总数涨到 7,284" src="docs/assets/charts/fleet-tiers-light.svg" width="800">
</picture>

</div>

## 工作原理

每个沙箱处在四个档位之一。闲得越久降得越低，每次降一档；任何请求都让它直接回到 hot。

| 档位 | 状态 | 占内存 | 回到 hot |
|---|---|---|---|
| **hot** | 运行中 | 是 | — |
| **frozen** | 原地暂停（`cgroup.freeze`） | 是 | 约 10 ms |
| **snapshot** | checkpoint 到节点本地盘 | 否 | 约 0.2 s |
| **detached** | 只剩它的卷 | 否 | 冷启动 |

- **一个 master，多个节点。** master 把期望状态存在 SQLite 里，决定沙箱放在哪台机器。节点负责运行沙箱并上报。可以由 master 凭地址和配对码连上节点，也可以由 NAT 后面的节点主动连 master。
- **节点会保护自己。** 内存不够或内存压力过高时，节点会拒绝新任务，不会把自己撑爆，ssh 也一直能连上。
- **流量唤醒。** 请求打到 frozen 或 snapshot 沙箱的端口，就会把它唤醒。
- **只有一个二进制。** `forkd-serve` 既是 master，也是节点，还带 web 面板。

## 实测数据

2026-10-26 实测：六个节点（AMD EPYC，每台 2 vCPU、1.9 GB），一台 master（2 vCPU），runsc `release-20261019.0`。

| | |
|---|---|
| 沙箱全部常驻 | **431** 个（每台约 72 个） |
| 闲置的沙箱自动降档 | **7,284** 个在运行，启动失败 0 个 |
| 冷启动到第一次 exec | p50 **0.24 s** |
| 从 snapshot 唤醒 | p50 **0.19 s** |
| 同时唤醒 300 个 snapshot | 300 个全部成功，用时 **7.3 s** |
| master 能接收的创建请求 | 约 **500 个/秒** |
| 集群能持续消化的创建速度 | **15–20 个/秒**；再快，节点就开始暂缓接活 |
| master 自身 | 内存 16 MiB，存储锁等待 p99 0.5 ms |

测试用的沙箱是一个 sleep 的 Python 进程，常驻约 14 MiB。为了让测试在几分钟内跑完，空闲计时调短了。每个节点上有一个 2 GB 的磁盘 swap 文件，zswap 是关的。

7,284 这个上限卡的不是沙箱内存。snapshot 档的沙箱还留着自己的网络命名空间，每个约占 0.4 MiB 内核内存，是这部分先用完了。原始数据、测试方法和所有轮次的结果在 [bench/fleet/results/2026-10-26](bench/fleet/results/2026-10-26/README.md)。

## 快速开始

需要 Linux 和 cgroup v2，`PATH` 里要有 [`runsc`](https://gvisor.dev/docs/user_guide/install/)，编译需要 Rust。

```bash
git clone https://github.com/deeplethe/forkd.git && cd forkd
cargo build --release -p forkd-core
sudo ./target/release/forkd-serve serve --master --node
```

这样会在一个进程里同时跑 master 和节点，面板在 `https://127.0.0.1:8080`，API token 在 `/var/lib/forkd/pki/api-token`。沙箱从模板启动，模板是一个 rootfs 加一份 OCI spec，用 `POST /v1/templates` 注册；最小的例子见 `scripts/fleet.sh seed`。

**多台能 ssh 上去的机器组成集群：**

```bash
scripts/fleet.sh up --master m1 --nodes n1,n2,n3 --listen 80
scripts/fleet.sh seed     # 在每个节点放一个模板
scripts/fleet.sh verify   # 启动一个沙箱，确认真的能跑
```

**已经装好的节点**，用 `FORKD_LISTEN=0.0.0.0:80` 监听。第一次启动时它会把配对码写到 `/var/lib/forkd/pki/node/pairing-code`。把地址和配对码交给 master：

```bash
curl --cacert ca.pem -H "Authorization: Bearer $FORKD_TOKEN" \
  https://master:8080/v1/nodes -d '{"address": "10.0.0.5", "pairing_code": "<配对码>"}'
```

配对码重启后不变，拿到它就能在那个节点上执行任何操作，请像 root 密码一样保管。

HTTP API 见 [docs/API.md](docs/API.md)。集群压测见 [bench/fleet](bench/fleet/README.md)。

## 状态

v0.6 还没有发布，改动很大：后端从 Firecracker 微虚拟机换成 gVisor，从单机变成集群。最近一次发布是 v0.5.3，那是基于 Firecracker 的 fork 运行时，它的 README 见[那个 tag](https://github.com/deeplethe/forkd/tree/v0.5.3)。设计文档在 [DESIGN-v0.6.md](DESIGN-v0.6.md)。

接下来：

- [ ] snapshot 档的沙箱释放网络命名空间，唤醒时再重建。这是现在限制密度的瓶颈。
- [ ] master 在分配时预留容量，避免一波突发请求超过节点上次上报的余量。
- [ ] 同一节点上并行恢复多个 snapshot。

## 许可证

[Apache-2.0](LICENSE)
