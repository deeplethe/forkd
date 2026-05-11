# Pitch: forkd

## The 30-second version

Modal's hidden moat is per-sandbox forking. They charge a lot for it because nothing else can do it. We're making it open source.

## Why now

Three things changed in the past 18 months:

1. **Agents got real.** Claude Code, Codex, Devin, AutoGen — code-writing agents produce real software at meaningful scale.
2. **Tree search + self-consistency moved from papers to products.** AlphaProof, AlphaCode 2, OpenAI's o1/o3 reasoning models — all rely on exploring multiple trajectories and picking the best. The expensive part isn't the model anymore; it's running N sandboxes.
3. **Modal proved the market.** $80M+ raised. Per-sandbox forking is their technical moat. Everyone else launches N independent sandboxes — slow, expensive, no shared state.

If you want to run 100 parallel agent trajectories cheaply today, your options are:

- **Modal** ($$$, closed source, vendor lock-in)
- **Build it yourself** (months of kernel-level work)
- **Don't do parallel exploration** (most teams' current "solution")

That's a wide-open gap.

## What `forkd` is

A single open primitive: **`fork()` for microVMs.**

```
parent.snapshot() → child1, child2, ..., childN
                    (share memory CoW, diverge on write)
```

One concept, one capability, one project. No platform, no SaaS, no lock-in.

## Who needs this

| Group | Need |
|---|---|
| Agent framework authors (LangGraph, CrewAI, AutoGen) | parallel agent execution primitive |
| AI coding products (Cursor, Cognition, Replit) | run N candidate solutions, pick best |
| Self-consistency research | cheap sampling at sandbox level |
| Teams that want self-hosted Modal | cost + sovereignty |
| Cloud providers | drop-in component for their sandbox products |

## What we won't do

- **Not a platform.** Open primitive only. No managed service, no billing, no UI.
- **Not GPU first.** CPU-only v0.1. GPU is v0.3+.
- **Not multi-host.** Single-host fork only in v0.1. Multi-host is v1.
- **Not a Modal clone.** We're a layer Modal could (and likely will) integrate.

## How we win

1. **First mover on the open primitive.** Get the name and the API mindshare before anyone else.
2. **Pick the right demo.** 100 parallel agents on SWE-bench, not synthetic benchmarks.
3. **Court the framework authors.** A `langgraph.fork()` integration ships before our v0.5.
4. **Donate to LF / CNCF.** Make it neutral infra, not a startup's wedge.
5. **Be opinionated.** One way to fork, one runtime (Firecracker), one host OS to start. Optionality kills early-stage infra projects.

## Why this can't be a closed-source company

- It's a primitive, not a product. People want it inside their stack, not as a hosted service.
- Modal owns the hosted-service market. They'll beat anyone on UX and GPU scale.
- The defensive moat is *being the standard*, not *being the vendor* — like Firecracker is to AWS.

So: open source from day 1, Apache 2.0, set it free.

## Risks

- **KSM / lazy-paging tuning harder than expected.** Mitigation: ship even at 5× memory share first; tune to 20×+ later.
- **Modal open-sources first.** Possible. Why we move fast.
- **Firecracker upstream rejects patches.** Mitigation: fork; consider Cloud Hypervisor as backup runtime.
- **Demo doesn't impress.** Mitigation: pick the workload carefully (large parent, observable parallelism, real task like SWE-bench).
