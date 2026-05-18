<br/>

<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
    <img alt="forkd" src="docs/logo.svg" width="220">
  </picture>
</div>

<br/>
<br/>

<p align="center">
  <a href="https://github.com/deeplethe/forkd/actions"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/deeplethe/forkd/ci.yml?branch=main&style=flat-square&label=ci"></a>
  <a href="https://github.com/deeplethe/forkd/releases"><img alt="Release" src="https://img.shields.io/github/v/release/deeplethe/forkd?style=flat-square&color=4c956c"></a>
  <a href="https://pypi.org/project/forkd/"><img alt="PyPI" src="https://img.shields.io/pypi/v/forkd?style=flat-square&color=3776ab&logo=pypi&logoColor=white"></a>
  <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square"></a>
  <a href="./README.md"><img alt="English README" src="https://img.shields.io/badge/README-English-blue?style=flat-square"></a>
  <a href="https://github.com/deeplethe/forkd/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/deeplethe/forkd?style=flat-square&color=eab308&logo=github"></a>
</p>

<br/>

## 101 毫秒,fork 出 100 个 microVM。

面向 **AI Agent 扇出**(fan-out)场景的 microVM 沙箱运行时。子 VM
从一个已"暖启动"的父快照 fork 而来,通过写时复制(CoW)继承
父进程的地址空间,而不是冷启动一个新内核。

forkd 基于 Firecracker 构建。父 VM 启动一次,把运行时(Python +
依赖,JIT 已暖的 JVM,已加载的 ML 模型)导入内存,然后暂停并落盘。
每个子 VM 是一个独立的 Firecracker 进程,通过 `MAP_PRIVATE`
方式 `mmap` 父快照的内存镜像;内核在页面级别实现写时复制,因此
在子 VM 发生写入分歧之前,它们共享父 VM 的常驻内存。

由此同时获得两个特性:**每个子 VM 都是独立的 KVM 隔离**,
同时**单个子 VM 的启动成本接近 `fork(2)`,而非冷启动 VM**。

<br/>

## Demo:让一个思考中的 agent 分裂

一个 24 秒的演示 —— 源 agent 在 LangGraph ReAct loop 中跑到一半,
被 BRANCH,3 个子沙箱继承同一份认知状态,各自收到不同的引导
("深度文化派"、"极简派"、"省钱派"),输出三条**显著不同的**
第一天行程。

![forkd 分裂思考中的 agent](./docs/assets/demo-zh.gif)

最关键的分裂证据:源 agent(无引导)第一天下午选 Nishiki Market
(锦市场,$$);3 个被引导的子沙箱**各自独立地都换成了 Arashiyama
Bamboo Grove**(岚山竹林,free);"省钱派" 还在 $$ 餐厅那里加上了
"may be pricey" 的警告语,其他两个没有。**模型并没有被告知"换景点"**
—— 是 hint 影响了下一次 LLM 调用,前置推理整套都没变。

完整机制 + 数据 + 原始 transcript 在
[`recipes/langgraph-react/`](./recipes/langgraph-react/) 和
[`recipes/langgraph-react/DEMO.md`](./recipes/langgraph-react/DEMO.md)。

<br/>

## 关键特性

- **硬件级隔离。** 每个子 VM 都是独立的 Firecracker microVM,
  基于 KVM。要逃逸出来需要 hypervisor 或内核漏洞,而不是
  `runc` 的一个回归 bug。
- **暖启动的运行时免费继承。** 导入、JIT 编译、模型权重、预取的
  缓存——只要父 VM 做过的事,子 VM 直接拿到。
- **每个子 VM 都是真 Linux。** 多 vCPU、完整 TCP 网络、`apt
  install`、出站 HTTPS。和那些为了极致启动速度牺牲掉
  单 vCPU + 串行 I/O 的函数级快照运行时不同,forkd 的子 VM
  可以跑真实的 Python 服务、模型推理或任何需要完整内核的负载。
- **从设计上就是多租户。** 每个子 VM 独立 network namespace、
  独立 cgroup v2 内存限制、独立 `/dev/urandom`(Linux 5.20+
  通过 `vmgenid` 重新播种)。
- **为 Agent 扇出而生。** 单次请求扇出到大量短生命周期沙箱的
  AI Agent 负载——代码解释器、工具调用、评估 rollout——是
  设计目标。暖启动的父 VM 把每次请求的 `import numpy` /
  `import torch` 开销在整个 cohort 间分摊到接近零。
- **可运维。** 守护进程持有状态、REST API(Unix 或 TCP)、
  Prometheus `/metrics`、append-only JSON 审计日志、systemd 单元。
- **开源。** Apache 2.0,没有厂商 SDK 锁定。

<br/>

## 基准测试

同一台 Linux 主机(Ubuntu 24.04,Linux 6.14,20 vCPU,30 GiB,KVM)。
负载:启动 100 个沙箱,每个执行 `import numpy;
numpy.zeros(5).tolist()`。

![Spawn time at N=100](./bench/chart-spawn-100.png)

![Host memory per sandbox](./bench/chart-memory-per.png)

| 后端 | N=100 总耗时 | 每个沙箱内存增量 | 说明 |
|---|---:|---:|---|
| **forkd** | **101 ms** | **0.12 MiB** | 从暖启动快照 CoW fork |
| CubeSandbox¹ | 1.06 s | 5 MiB | RustVMM microVM,冷启动(池 fast path) |
| BoxLite² | 113.2 s | — | KVM microVM,冷启动 OCI rootfs |
| OpenSandbox³ | 122.0 s | — | 经抽象层调用 Docker 运行时 |
| Firecracker 冷启动 | 759 ms | 84 MiB | 裸 VM 启动,无编排 |
| gVisor (runsc) | 288.6 s | — | 用户态内核容器 |
| Docker (runc) | 335.3 s | 4 MiB | 标准容器运行时 |

¹ CubeSandbox:1.06 s 是本机的 **fast-path** N=100 数字(1056 ± 14 ms,
五次连跑,每次 100 % 成功),用 pre-warm Python `ThreadPoolExecutor`
的改进 bench 脚本测,避免 client 侧懒初始化污染测量。之前在同一台
机器上跑的 slow-path 是 20.3 s / 77 成功 —— 那次模板的 writable
layer 是 2 GiB,与默认 1 GiB 池不匹配,每个沙箱都走了一遍
`mkfs.ext4 + reflink-copy`;维护者在
[#235](https://github.com/TencentCloud/CubeSandbox/issues/235)
澄清后,我们把 `2Gi` 加进 `pool_default_format_size_list` 重测。
机器跑的是 cube **v0.2.0**,这版有一个 ~50 ms 延迟回归,
[PR #234](https://github.com/TencentCloud/CubeSandbox/pull/234)
在 v0.2.1 中修复 —— 上面的数字是 v0.2.0 baseline。CubeSandbox
公布单实例冷启动 **<60 ms**(96 vCPU 主机,N=100 并发 P99 200 ms),
我们没在此处复测那个形态。详见
[`bench/CUBESANDBOX.md`](./bench/CUBESANDBOX.md)、以及两个上游 PR
[#236](https://github.com/TencentCloud/CubeSandbox/pull/236) /
[#237](https://github.com/TencentCloud/CubeSandbox/pull/237)。

² BoxLite 的设计目标是每个负载一个长生命周期、有状态的 Box,
而不是 100 个并发的全新 microVM。冷启动扇出数据放在这里仅为
直接对比。详见 [`bench/BOXLITE.md`](./bench/BOXLITE.md)。

³ OpenSandbox 是 Docker / K8s / gVisor / Kata / Firecracker 上层的
抽象层;此处数字是它默认的 Docker 运行时。详见
[`bench/OPENSANDBOX.md`](./bench/OPENSANDBOX.md)。

复现:`bench/bench-spawn-100.sh` 然后 `bench/generate_charts.py`。

对单个沙箱执行同一个 numpy 表达式的两种方式:

| 调用 | 耗时 | 做了什么 |
|---|---:|---|
| `sandbox.eval("numpy.zeros(5).tolist()")` | 1 ms | 复用 PID 1 里已暖的 Python |
| `sandbox.commands.run("python3 -c '...'")` | 96 ms | 冷子进程重新 import numpy |

<br/>

## 工作原理

```mermaid
flowchart TB
    %% ─── parent ───────────────────────────────────────────────
    subgraph PARENT["父 VM(启动一次并暖启动)"]
        direction TB
        runtime["PID 1<br/>Python + numpy + 你的依赖<br/>已导入 RAM"]
    end

    PARENT -- "暂停 + 快照" --> SNAP["磁盘上的快照<br/>memory.bin(CoW 源)<br/>vmstate(vCPU + 设备)"]

    %% ─── controller ───────────────────────────────────────────
    CLIENT["客户端(CLI / Python SDK)"] -- "POST /v1/sandboxes n=100" --> CTL["forkd-controller<br/>REST · 鉴权 · 审计 · /metrics"]
    CTL -- "restore_many_with(...)" --> SNAP

    %% ─── children ─────────────────────────────────────────────
    subgraph CHILDREN["100 个子 Firecracker 进程(内核每页 CoW)"]
        direction LR
        subgraph NS1["netns forkd-child-1"]
            C1["子 VM 1<br/>mmap MAP_PRIVATE<br/>cgroup memory.max"]
        end
        subgraph NS2["netns forkd-child-2"]
            C2["子 VM 2<br/>mmap MAP_PRIVATE<br/>cgroup memory.max"]
        end
        subgraph NSN["netns forkd-child-100"]
            CN["子 VM 100<br/>mmap MAP_PRIVATE<br/>cgroup memory.max"]
        end
    end

    SNAP -. "共享文件<br/>(基本只读)" .-> C1
    SNAP -. "共享文件" .-> C2
    SNAP -. "共享文件" .-> CN

    %% ─── network ──────────────────────────────────────────────
    C1 -- "veth" --> BR["宿主机桥 forkd-br0<br/>MASQUERADE"]
    C2 -- "veth" --> BR
    CN -- "veth" --> BR
    BR --> UPLINK(("uplink → 公网"))

    %% styling
    classDef parent fill:#e8f3ec,stroke:#4c956c,color:#1f2933;
    classDef snap   fill:#fff3df,stroke:#d4a259,color:#1f2933;
    classDef ctl    fill:#e6efff,stroke:#5b7dba,color:#1f2933;
    classDef child  fill:#ffffff,stroke:#52606d,color:#1f2933;
    classDef net    fill:#f1f3f5,stroke:#8d99ae,color:#1f2933;
    class PARENT,runtime parent;
    class SNAP snap;
    class CTL,CLIENT ctl;
    class NS1,NS2,NSN,C1,C2,CN child;
    class BR,UPLINK net;
```

完整设计、以及当前架构留下的开放问题,见
[`DESIGN.md`](./DESIGN.md)。

<br/>

## forkd 与其他方案对比

沙箱运行时这个领域的设计差异很大。下表把 forkd 和最常被提及的
开源项目放在一起对比。引号内的数字是**上游项目自己公布的数字**,
除非已经在上面我们的基准图里。forkd 不会去测别的项目在它们
本来就不为之设计的负载形态。

| 项目 | 隔离原语 | 冷启动 (N=100) | Fork 自暖快照 | 配额 | 鉴权 / TLS | 协议 |
|---|---|---|:---:|---|---|---|
| **forkd** | Firecracker + 快照 CoW | **101 ms** | ✓ | cgroup `memory.max` | bearer + rustls | Apache 2.0 |
| [CubeSandbox][cs] | RustVMM + KVM microVM | 1.06 s¹ | "coming soon" | <5 MiB / 实例 | 闭源不开放 | Apache 2.0 |
| [Daytona][dy] | OCI workspace | <90 ms² | ✗ | 每 workspace | 平台 API key | **AGPL-3.0** |
| [OpenSandbox][os] | Docker / K8s + gVisor / Kata / FC | 122 s | ✗ | 取决于底层 | k8s 网关 | Apache 2.0 |
| [E2B][e2b] | Firecracker(在 [infra][e2b-infra] 中) | 开源不含 | ✗ | 平台侧 | 云 API key | Apache 2.0 |
| [BoxLite][bl] | KVM / Hypervisor.framework + OCI | 113 s | ✗ 有状态 Box | KVM + seccomp | 仅出站策略 | Apache 2.0 |
| Modal | 闭源快照 fork | 不公开 | ✓ | ✓ | ✓ | 闭源 |
| Firecracker 裸用 | 只有 microVM | 759 ms | 手工 | n/a | n/a | Apache 2.0 |
| Docker (runc) | OCI 容器 | 335 s | ✗ | cgroups | n/a | Apache 2.0 |
| gVisor (runsc) | 用户态内核 | 289 s | ✗ | cgroups | n/a | Apache 2.0 |

¹ N=100 并发,本机裸金属(`systemd-detect-virt: none`,i7-12700,
20 vCPU,无嵌套虚拟化)。这是 **fast path** 的数字 ——
`pool_default_format_size_list` 已包含模板的 writable-layer 尺寸,
每个沙箱复用预格式化的池条目,不走 `mkfs.ext4 + reflink-copy`。
五次连跑均值 1056 ± 14 ms,每次 100 % 成功,bench 脚本会先 pre-warm
Python 的 `ThreadPoolExecutor`,把 client 侧懒初始化排除在测量外。
机器跑的是 cube **v0.2.0**,这版有 ~50 ms 延迟回归,
[PR #234](https://github.com/TencentCloud/CubeSandbox/pull/234) 在
v0.2.1 修 —— 上面的数字是 v0.2.0 baseline。之前在同一台机器上的
slow-path 测得 20.3 s / 77 成功 —— 那是我们模板配错的结果,维护者
在 [#235](https://github.com/TencentCloud/CubeSandbox/issues/235)
指出后修正。Cube 自称单实例冷启动 **<60 ms**(96 vCPU 主机,
N=100 并发 P99 200 ms),我们没复测。注意这行是把 **forkd 的
fork-from-warm** 跟其他项目的 **冷启动** 放在一起 —— 它们是不同
的工作点,不是等价原语。详见
[bench/CUBESANDBOX.md](./bench/CUBESANDBOX.md),以及为上游 cmdTimeout
race 提的两个 PR
[#236](https://github.com/TencentCloud/CubeSandbox/pull/236) /
[#237](https://github.com/TencentCloud/CubeSandbox/pull/237)。

² Daytona 自己公布的数字,我们没测它(workspace 运行时,不属于
可对比的扇出形态)。

[cs]: https://github.com/TencentCloud/CubeSandbox
[dy]: https://github.com/daytonaio/daytona
[os]: https://github.com/alibaba/OpenSandbox
[e2b]: https://github.com/e2b-dev/E2B
[e2b-infra]: https://github.com/e2b-dev/infra
[bl]: https://github.com/boxlite-ai/boxlite

**forkd 适合什么场景。**

- **代码解释器和 Jupyter kernel 沙箱。** 每一次对话或工具调用
  都开一个全新 kernel;暖启动的父 VM 带着 SciPy / ML 运行时,
  所以每次请求的 `import numpy` / `import torch` 直接降到零开销。
  这就是设计目标——Anthropic / OpenAI / Modal 的代码解释器
  产品全是这种负载形态。
- **评估测试集 harness。** 几百个仓库 checkout 或测试 rollout
  并行跑——SWE-bench 那种形状——又不用每个 task 付 Docker
  冷启动的代价。
- **大规模扇出的多用户代码执行。** 大量短生命周期沙箱共享
  同一个暖启动父 VM,每个子 VM 都是 KVM 隔离。
- **CI 里执行不可信代码。** `git clone`、`pip install`、
  `pytest` 跑在真实的 Linux VM 里,而不是一个容器 namespace。
- **托管沙箱 SaaS 的自托管替代品。** 一台 Linux + KVM,单二进制
  守护进程,Apache 2.0——没有按秒计费的云账单,也没有厂商锁定。

**别的项目更合适什么。** CubeSandbox:更快的纯冷启动(自称
<60 ms)。Daytona:每个用户拥有一个长生命周期沙箱的 workspace
形态。OpenSandbox:用一套编排 API 适配多种隔离后端。BoxLite:
可嵌入、不需要守护进程、跨平台(macOS 走 Hypervisor.framework)。
Modal:同一种原语的闭源托管版本。

**forkd 的不适用场景。** 函数级快照运行时为了极致启动速度
放弃了真实 Linux(单 vCPU、只有串行 I/O),它们能在 forkd 的
~100 ms 基础上再快一个数量级——代价是跑不了真实的 Python
服务、`apt install`、出站 HTTPS。

<br/>

## 企业部署 FAQ

给平台 / 采购团队的速查答案:

**能直接上 Kubernetes 吗?** 可以——**一个 forkd-controller Pod 承载 N 个沙箱子 VM**,K8s 调度器只在 Pod 创建时跑**一次**,跟扇出数量无关(对比 Kata / Firecracker-on-K8s 那种"每个沙箱一个 Pod"的设计)。起步 manifest 在 [`packaging/k8s/`](./packaging/k8s/)。节点要有 `/dev/kvm` + cgroup v2;托管 K8s(GKE / EKS / AKS)通常得选裸金属 SKU 或显式开嵌套虚拟化才行。

**一个 Pod 能塞多少沙箱?** 用 512 MiB 暖好的 Python+numpy 父 VM,大致定容:

- **每 vCPU 跑 ~1 个**活跃 agent(算力瓶颈)
- **每 8 GiB Pod RAM 装 ~50 个**空闲池里的 agent(进程状态瓶颈,**不是**内存)

N=100 实测 CoW 开销是 **每个 child 0.12 MiB**(详见 [bench/](./bench/)),内存几乎从来不是扇出的天花板,真正卡住的是 vCPU 和进程数。父 VM 更重的负载(浏览器、ML 推理)更快撞顶,具体定容请用自家父 VM 实测。

**现有 agent 怎么接入?**

- **REST** —— `POST /v1/sandboxes n=100`,跟语言无关,bearer-token 鉴权
- **Python SDK** —— `from forkd import Sandbox`(可替 `from e2b import Sandbox`)
- **LangGraph / AutoGen / CrewAI** —— 通过 Python SDK,无需特殊适配
- **MCP** —— `pip install forkd-mcp` 提供 MCP server,可接入 Claude Desktop / Claude Code / Cursor / Cline,详见 [`sdk/mcp/`](./sdk/mcp/)

**生产场景形态(对应仓内 recipe):**

- **AI 代码解释器** —— 一个暖好的父 VM(SciPy / torch 已 import),每个对话回合 fork 一个 child。Recipe:[`e2b-codeinterpreter/`](./recipes/e2b-codeinterpreter/)
- **SWE-bench 风格并行评测** —— N 个并行 repo checkout,每个 child 独立跑 `pytest`。Recipe:[`coding-agent/`](./recipes/coding-agent/)
- **多用户代码执行规模化** —— 共享暖父 VM,每个用户的 child 由 KVM 隔离
- **CI 跑不可信代码** —— `git clone + pip install + pytest` 在真 Linux VM 里跑,不是容器 namespace
- **每测试隔离数据库** —— Recipe:[`postgres-fixture/`](./recipes/postgres-fixture/) —— 每个 child ~10 ms 拿到 ready-to-query postgres,跳过 ~2 s 的 fresh `initdb`

<br/>

## 快速开始

要求:x86_64 Linux,带 KVM,Ubuntu 22.04 或更新。

```bash
# 1. 主机准备:KVM、Firecracker、Rust、KSM、大页、tap 设备。
sudo bash scripts/setup-host.sh
sudo bash scripts/host-tap.sh
cargo build --release
sudo install -m 0755 target/release/{forkd,forkd-controller} /usr/local/bin/

# 2. 从一个 Docker 镜像构建暖启动 rootfs。
sudo bash scripts/build-rootfs.sh python:3.12-slim python-rootfs.ext4 1536 python3-numpy

# 3. 拉一个内核。
curl -O https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/vmlinux-6.1.141

# 4. 跑一个一次性沙箱。
sudo -E forkd run --image python:3.12-slim --kernel ./vmlinux-6.1.141 \
    -- python3 -c "import numpy; print(numpy.zeros(5).sum())"
# 0.0
```

### 多子 VM 扇出

```bash
# 一次性给 N 个子 VM 准备好网络 namespace。
sudo bash scripts/netns-setup.sh 100

# 创建一个带 tag 的父快照。
sudo forkd snapshot --tag pyagent \
    --kernel ./vmlinux-6.1.141 \
    --rootfs ./python-rootfs.ext4 \
    --tap forkd-tap0

# Fork 100 个共享父 VM 内存的子 VM。
sudo -E forkd fork --tag pyagent -n 100 --per-child-netns --memory-limit-mib 256

# 和其中一个子 VM 通信。
sudo forkd eval --child forkd-child-42 -- "numpy.zeros(100).sum()"
```

### Python SDK

```python
from forkd import Sandbox   # 可直接替换 `from e2b import Sandbox`

with Sandbox() as sb:
    print(sb.commands.run("uname -a").stdout)
    print(sb.eval("numpy.zeros(5).tolist()"))    # 复用暖启动的 PID 1
```

### MCP server

用 Claude Desktop / Claude Code / Cursor 等任意
[MCP](https://modelcontextprotocol.io/) 客户端接入:

```bash
pip install forkd-mcp
# 然后在 claude_desktop_config.json 里加:
#   "mcpServers": { "forkd": { "command": "forkd-mcp" } }
```

Server 暴露 `spawn_sandboxes`、`exec_command`、`eval_code` 等 8 个
工具,agent 可以直接驱动 forkd 微 VM。详见
[`sdk/mcp/README.md`](./sdk/mcp/README.md)。

### 预构建 recipe

不想自己设计 rootfs?直接从 [`recipes/`](./recipes/) 选一个,
跑它的 `build.sh`:

| Recipe | 适合什么 |
|---|---|
| [`python-numpy/`](./recipes/python-numpy/) | 复现基准测试;最轻量的 Python + numpy |
| [`e2b-codeinterpreter/`](./recipes/e2b-codeinterpreter/) | AI 代码解释器 agent(E2B SDK 兼容) |
| [`jupyter-kernel/`](./recipes/jupyter-kernel/) | 预导入 notebook / SciPy 栈;每个 kernel ~1 ms |
| [`coding-agent/`](./recipes/coding-agent/) | SWE-bench / 编码 agent,带 `git` + 开发工具 |
| [`nodejs/`](./recipes/nodejs/) | JS / TS 负载,Playwright 扇出 |
| [`playwright-browser/`](./recipes/playwright-browser/) | 驱动浏览器的 agent(computer-use / 网页研究 / UI 测试生成),fork 出已暖好的 Chromium ≈10 ms |
| [`agent-workbench/`](./recipes/agent-workbench/) | 全家桶——浏览器 + VSCode + Jupyter + MCP |
| [`postgres-fixture/`](./recipes/postgres-fixture/) | fork-per-test 隔离数据库,每个 child ~10 ms 拿到 ready-to-query postgres,跳过 ~2 s fresh initdb |

<br/>

## 守护进程模式部署

Controller 守护进程持有快照和活跃沙箱的注册表,对外暴露 REST API,
并写结构化审计日志。除了本地开发,推荐都用这种模式部署。

```bash
sudo install -m 0644 packaging/systemd/forkd-controller.service /etc/systemd/system/
sudo mkdir -p /etc/forkd
sudo bash -c 'head -c 32 /dev/urandom | base64 > /etc/forkd/token'
sudo chmod 600 /etc/forkd/token
sudo systemctl enable --now forkd-controller
```

然后用 HTTP 驱动它:

```bash
TOKEN=$(sudo cat /etc/forkd/token)
curl -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8889/v1/sandboxes \
     -H 'Content-Type: application/json' \
     -d '{"snapshot_tag":"pyagent","n":5,"per_child_netns":true,"memory_limit_mib":256}'
# [{"id":"sb-67a1b3-0000","pid":...,...}, ...]

curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8889/metrics
# forkd_sandboxes_active 5
```

完整 API 参考:[`docs/API.md`](./docs/API.md)。
运维手册:[`docs/RUNBOOK.md`](./docs/RUNBOOK.md)。
安全姿态:[`docs/SECURITY.md`](./docs/SECURITY.md)。

<br/>

## 仓库结构

```
crates/
  forkd-vmm/            Firecracker 封装:BootConfig、Vm、Snapshot、cgroup
  forkd-cli/            `forkd` 二进制(snapshot、fork、run、exec、eval)
  forkd-controller/     `forkd-controller` 守护进程:REST、注册表、审计
rootfs-init/
  forkd-init.sh         guest 内的 PID 1;挂载伪文件系统,启动 agent
  forkd-agent.py        guest 内 :8888 上的 TCP server(ping/exec/eval)
sdk/python/             E2B 兼容的 Python SDK
sdk/mcp/                MCP server(`forkd-mcp`)—— 从 Claude
                        Desktop / Claude Code / 任何 MCP 客户端驱动 forkd
scripts/                宿主机侧的辅助脚本(KVM、Firecracker、netns、rootfs)
packaging/systemd/      Controller 的 systemd unit
packaging/k8s/          forkd-controller 的 Kubernetes starter manifest
recipes/                预构建的父 rootfs recipe(python-numpy、
                        e2b-codeinterpreter、coding-agent、nodejs、
                        agent-workbench)。详见 recipes/README.md。
bench/                  基准测试 harness、图表生成器、结果
docs/                   API.md、SECURITY.md、RUNBOOK.md
```

<br/>

## 状态

Alpha。fork-on-write 原语、controller 守护进程、REST API、
鉴权、审计日志、cgroup 内存限制、Prometheus metrics、
Python SDK 都已就绪,并由 CI 里的 25 个单元 + 集成测试覆盖。
1.0 之前,磁盘格式和 API 形态可能还会有变化。

本版本暂未达到生产可用的项:

- 多节点调度(目前 一个守护进程 = 一台主机)。
- 子 VM netns 的默认拒绝出站(目前是共享 MASQUERADE 规则;
  想要 allow-list 策略的用户需要自己给每个 netns 加 `iptables` 规则)。
- `memory.max` 之外的 `cpu.max`、`io.max`、`pids.max` 配额。
- 第三方安全审计。

Roadmap 和正在追踪的工作都在 [GitHub issues](https://github.com/deeplethe/forkd/issues)。
版本变更记录:[CHANGELOG.md](./CHANGELOG.md)。
安全策略与历史漏洞通告:[docs/SECURITY.md](./docs/SECURITY.md)。

**v0.3 phase 1 已 ship** —— diff-snapshot BRANCH 把源 VM 的暂停
时间从 **29.3 秒砍到 205 毫秒（143x）**(4 GiB SSD 源,空闲)。
完整表格和诚实 caveat 见
[`bench/pause-window/RESULTS-v0.3.md`](./bench/pause-window/RESULTS-v0.3.md);
60 个 trial 的 sweep 原始数据在 `bench/pause-window/diff-real-sweep-*.csv`。
通过 `POST /v1/sandboxes/:id/branch` 请求体加 `"diff": true` 开启。

胜的是源 VM 的**停机时间**,不是 BRANCH API 的总延迟:source
的 memory.bin cp 在后台和 source 并行跑,然后 diff 窗口关闭、
source 恢复、diff 合到事先 cp 出来的输出上。源 VM 的 TCP 连接、
kvmclock、定时器只看到 ~200ms 的 gap 而不是 29 秒。BRANCH API
本身的总时间还是被 cp 带宽限制——这个 trade-off 对"长跑 agent
的 live BRANCH"是甜区。

限制: v0.3.0 的 diff 模式只支持 sandbox 的**第一次** BRANCH
(Firecracker 在每次 snapshot/create 都清脏页 bitmap;多次
BRANCH 需要 per-sandbox shadow file,defer 到 v0.3.1+)。
设计文档和 defer 原因:
[`docs/design/diff-snapshots.md`](./docs/design/diff-snapshots.md)。

更大的 v0.4+ 候选——基于 memfd + uffd_wp 的 live-fork——还在
[issue #101](https://github.com/deeplethe/forkd/issues/101)。
scaffolding (`crates/forkd-uffd/`、`firecracker-patch/`、
`MemoryBackend::Userfault` enum) 留作起点。

> **0.1.4 包含 daemon 侧安全修复**。`POST /v1/sandboxes` 的
> `snapshot_tag` 校验缺失(任意路径 → 控制 grandchild VM
> volumes)和 K8s manifest 接受字面占位符 bearer token —— 两个 HIGH
> 影响 0.1.0–0.1.3,请升级。完整公告见
> [docs/SECURITY.md#past-advisories](./docs/SECURITY.md#past-advisories)。
> 0.1.3 此前还修复了 CLI `--tag` path-traversal(影响 0.1.0–0.1.2)。

<br/>

## 贡献

欢迎 Pull Request。开 PR 之前请:

1. 先开 issue 描述你想改什么。API 还在动,我们更愿意提前对齐
   而不是让你重写补丁。
2. 本地跑过 `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all`。
3. 提交带 sign-off(`git commit -s`)。

<br/>

## Star 历史

<a href="https://star-history.com/#deeplethe/forkd&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=deeplethe/forkd&type=Date&theme=dark">
    <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=deeplethe/forkd&type=Date">
  </picture>
</a>

<br/>

## License

Apache 2.0。详见 [LICENSE](./LICENSE) 与 [NOTICE](./NOTICE)。
