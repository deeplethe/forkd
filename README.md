<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="Sandstorm" src="docs/logo.svg" width="383">
</picture>

</div>

<br/>

<div align="center">

[How it works](#how-it-works) · [Numbers](#numbers) · [Quick start](#quick-start) · [Status](#status)

[![CI](https://img.shields.io/github/actions/workflow/status/deeplethe/sandstorm/ci.yml?branch=dev&style=flat-square&label=CI&labelColor=161B22)](https://github.com/deeplethe/sandstorm/actions)
[![License](https://img.shields.io/badge/LICENSE-APACHE%202.0-3FB950?style=flat-square&labelColor=161B22)](LICENSE)
[![Rust](https://img.shields.io/badge/BUILT%20WITH-RUST-F74C00?style=flat-square&labelColor=161B22&logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org)
[![DeepLethe](https://img.shields.io/badge/BUILT%20BY-DEEPLETHE-F1F2F5?style=flat-square&labelColor=161B22)](https://deeplethe.com)
[![gVisor](https://img.shields.io/badge/ISOLATION-GVISOR-2D333B?style=flat-square&labelColor=161B22&logo=data:image/svg%2bxml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSIxNzUuMzUiIGhlaWdodD0iMTkzLjIwIiB2aWV3Qm94PSIwIDAgMTc1LjM1IDE5My4yMCIgPjxnIHRyYW5zZm9ybT0ibWF0cml4KDEuMzMzMzMzMywwLDAsLTEuMzMzMzMzMywtNjE0LjQ1LDYzOC45NikiPjxnIHRyYW5zZm9ybT0idHJhbnNsYXRlKDU0OC4yNCwzNjMuMjQpIj48cGF0aCBkPSJtIDAsMCBjIDE2LjY1LDIxLjEyIDIyLjY5LDQ0LjQzIDE4LjMyLDcwLjk5IDMuMDYsMCA1Ljc0LC0wLjAyIDguNDEsMC4wMCAyLjIyLDAuMDIgNC40NCwwLjEwIDYuNjYsMC4xNyA0Ljc5LDAuMTUgNC44MSwwLjE2IDUuODgsLTQuNTggMy4xNCwtMTQuMDUgMi4xOCwtMjguMDkgLTAuMTcsLTQyLjExIC0wLjI1LC0xLjQ5IC0wLjcsLTIuOTUgLTAuODYsLTQuNDUgQyAzNy4wNSw5LjA4IDMwLjA4LDMuNjQgMjAuMTYsMS4wOSAxMy43OCwtMC41NCA3LjMyLC0wLjgyIDAsMCIgc3R5bGU9ImZpbGw6I2ZmZmZmZiIgLz48L2c%2BPGcgdHJhbnNmb3JtPSJ0cmFuc2xhdGUoNTQ0LjY4LDM5Ni4xNykiPjxwYXRoIGQ9Ik0gMCwwIEMgMCwyLjU5IC0zLjQ1LDQuMzIgLTMuNDUsNC4zMiAtMC44Niw1LjE4IDAsOC42NCAwLDguNjQgMCw4LjY0IDAuODYsNS4xOCAzLjQ1LDQuMzIgMy40NSw0LjMyIDAsMi41OSAwLDAgbSAtMTcuMDksNi40NSBjIDAsNi43NCAtOC45OCwxMS4yMyAtOC45OCwxMS4yMyA2Ljc0LDIuMjQgOC45OCwxMS4yMyA4Ljk4LDExLjIzIDAsMCAyLjI0LC04Ljk5IDguOTksLTExLjIzIDAsMCAtOC45OSwtNC40OSAtOC45OSwtMTEuMjMiIHN0eWxlPSJmaWxsOiNmZmZmZmYiIC8%2BPC9nPjxnIHRyYW5zZm9ybT0idHJhbnNsYXRlKDQ4NS4wOCw0MjkuNzkpIj48cGF0aCBkPSJtIDAsMCBjIDAsMCAtMi4yMSwzLjM1IC05LjczLDIuMDUgMCwwIDUuOTgsMjguNzMgMzYuMjksMzguODAgQyAyNi41Niw0MC44NiAtMi44MiwyNC4yNyAwLDAgTSAwLjU4LC0zMy4yMCAwLjU4LC0zMy4yMSBjIC0xLjI5LC0yLjAyIC0yLjgyLC0zLjU3IC00LjUzLC00LjYxIC0xLjUxLC0wLjkzIC0zLjE3LC0xLjQyIC00Ljc0LC0xLjQyIC0wLjUyLDAgLTEuMDQsMC4wNSAtMS41MywwLjE3IGwgLTIuMjAsMC41MCAyLjE4LDAuNjAgYyAxLjYzLDAuNDUgMy4wNiwxLjM0IDQuMjUsMi42NiAxLjAxLDEuMTAgMS44NSwyLjQ4IDIuNTcsNC4yMSAxLjE3LDIuODIgMS43OCw2LjE2IDEuODEsOS45MyAtMC4wNCwzLjcxIC0wLjY4LDcuMDUgLTEuODgsOS45MCAtMC43MCwxLjY1IC0xLjU4LDMuMDUgLTIuNjIsNC4xNiAtMS4yMiwxLjMxIC0yLjY3LDIuMTcgLTQuMjksMi41NyBsIC0yLjE4LDAuNTMgMi4xNywwLjU3IGMgMC40OCwwLjEyIDAuOTksMC4xOSAxLjUzLDAuMjAgaCAwLjAzIGwgMS42OCwtMC4xNSBjIDEuMDQsLTAuMjAgMi4xMCwtMC42MSAzLjA2LC0xLjE4IDEuNzUsLTEuMDMgMy4zMCwtMi41NiA0LjU5LC00LjU2IDIuMTUsLTMuMzcgMy4zMSwtNy41MyAzLjM1LC0xMi4wNCAtMC4wMiwtNC41NCAtMS4xNSwtOC43MSAtMy4yNywtMTIuMDYgbSAxMDEuOTQsNTEuMTcgYyAwLDAgLTAuMDcsMC4wNSAtMC4yMiwwLjEzIC0wLjU0LDAuMzIgLTEuMTEsMC42MCAtMS43MSwwLjgxIC0zLjA0LDEuMjQgLTkuODgsMy4xOCAtMjEuOTAsMi42MiAtMC4wMiwwIC0wLjA1LDAgLTAuMDgsLTAuMDAgQyA1Mi45MSwyMS4xMiAzMC4wMiwxMC45MiAzMC4wMiwxMC45MiBjIDAsMCAxLjQzLDEuNzYgMy40NSwzLjY5IDEwZS00LDAgMTBlLTQsMTBlLTQgMC4wMCwxMGUtNCAxLjA1LDAuOTcgMi4zNSwyLjA3IDMuOTEsMy4yMiAwLjA0LDAuMDMgMC4wOCwwLjA2IDAuMTIsMC4wOSA4LjcwLDYuMzggMjUuMzQsMTQuMTYgNTEuNjIsOS41NCAtMC45OCwxLjEyIC0yLjAwLDIuMTkgLTMuMDMsMy4yMSAtMS4xMSwwLjg4IC0yLjIzLDEuNjkgLTMuMzUsMi40NSAwLjAyLC0wLjAxIDAuMDMsLTAuMDIgMC4wNSwtMC4wMyAwLDAgLTE3LjAxLDE5LjQxIC00OC42OCwxNS44OSBDIDMwLjE5LDQ4LjYyIDI1Ljk4LDQ3Ljg2IDIxLjY2LDQ2LjU2IDIxLjY1LDQ2LjU2IDIxLjY0LDQ2LjU2IDIxLjY0LDQ2LjU2IEwgMjEuNjMsNDYuNTUgQyAxMS40OCw0My40OSAwLjY4LDM3LjM4IC04LjcxLDI1LjkxIC05LjI5LDI1LjE5IC05Ljg0LDI0LjQ3IC0xMC4zNywyMy43NyBjIC0wLjA2LC0wLjA5IC0wLjE0LC0wLjIwIC0wLjIyLC0wLjMwIC0xLjE4LC0xLjU4IC0yLjI3LC0zLjE0IC0zLjI3LC00LjY3IC0zLjI2LC01LjQyIC01LjYzLC0xMS42NiAtNi4zMSwtMTMuNTQgLTEwLjU4LC0zMi40MCAyLjU4LC01Ny41NSA1LjE0LC02MS45NiA4LjkzLC0xNS4xNSAyNC41NiwtMzIuMzUgNTAuNzcsLTM3LjMyIDAuMTksLTAuMDQgMC4zOCwtMC4xMCAwLjU4LC0wLjE0IDEuNzIsLTAuMzYgNC44NiwtMC45MiA4LjkwLC0xLjE5IDEuNTIsLTAuMDYgMy4wOCwtMC4wOSA0LjY5LC0wLjA2IDEuNTQsLTAuMDAgMi45OSwwLjAxIDQuMzMsMC4wNiAwLjM5LDAuMDIgMC43NCwwLjA0IDEuMDQsMC4wNCA1LjEwLDAuMjMgOC4yNywwLjc3IDguMjcsMC43NyAtMjYuNTQsMS4yOSAtMzkuODQsMTMuNDAgLTQ1LjY5LDIxLjE0IC0xLjMyLDEuNjQgLTIuNDYsMy40MiAtMy4zNyw1LjMxIC0wLjM2LDAuNzEgLTAuNTIsMS4xMSAtMC41MiwxLjExIDguNDUsLTcuMTggMjAuMjksLTEzLjM2IDIwLjI5LC0xMy4zNiAxMC42MSwtNC45OSAyMS43MywtNy40NSAzMy41MiwtNS44MyAwLDAgMjQuNjQsMi4yNiAzNC40NiwyNS4wOSAtMC40MiwwLjMyIC0wLjM2LDAuMjcgLTAuNzksMC42IC0wLjY1LC0wLjUyIC0xLjI5LC0xLjAyIC0xLjkyLC0xLjQ5IC0wLjk5LC0wLjY1IC0xLjk2LC0xLjM1IC0yLjkzLC0yLjA3IC01LjI2LC0zLjQ4IC05LjU2LC01LjEyIC0xMi4xMiwtNS44NyAtMTkuMzUsLTQuODggLTM3LjI3LC0xLjI1IC01Mi45MywxMi40NSAtNy4yNSw2LjM0IC0xMS43NSwxNC4zNSAtMTEuOTQsMjQuMjkgLTAuMTIsNi40NiAtMC4xOSwxMi45MyAtMC4xMywxOS40IDAuMDgsMTAuMTggNy4yNCwxNy45MiAxNy4zOSwxOS4yOCAyMC41NiwyLjc1IDQxLjIzLDMuNzEgNjEuOTQsNC4xOSAzLjE3LDAuMDcgNS45OCwwLjAzIDcuNDUsLTMuMzUgMC4wMiwtMC4wNSAwLjM2LDAuMDIgMC41NSwwLjAzIDEuNCw0LjQ2IC0wLjYxLDkuMjcgLTQuMjksMTEuNjIiIHN0eWxlPSJmaWxsOiNmZmZmZmYiIC8%2BPC9nPjwvZz48L3N2Zz4%3D)](https://gvisor.dev)
[![Firecracker](https://img.shields.io/badge/ISOLATION-FIRECRACKER-2D333B?style=flat-square&labelColor=161B22&logo=data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACgAAAAoCAYAAACM%2FrhtAAAAAXNSR0IArs4c6QAAAHhlWElmTU0AKgAAAAgABAEaAAUAAAABAAAAPgEbAAUAAAABAAAARgEoAAMAAAABAAIAAIdpAAQAAAABAAAATgAAAAAAAABgAAAAAQAAAGAAAAABAAOgAQADAAAAAQABAACgAgAEAAAAAQAAACigAwAEAAAAAQAAACgAAAAApjS%2BXAAAAAlwSFlzAAAOxAAADsQBlSsOGwAAAWRpVFh0WE1MOmNvbS5hZG9iZS54bXAAAAAAADx4OnhtcG1ldGEgeG1sbnM6eD0iYWRvYmU6bnM6bWV0YS8iIHg6eG1wdGs9IlhNUCBDb3JlIDYuMC4wIj4KICAgPHJkZjpSREYgeG1sbnM6cmRmPSJodHRwOi8vd3d3LnczLm9yZy8xOTk5LzAyLzIyLXJkZi1zeW50YXgtbnMjIj4KICAgICAgPHJkZjpEZXNjcmlwdGlvbiByZGY6YWJvdXQ9IiIKICAgICAgICAgICAgeG1sbnM6eG1wPSJodHRwOi8vbnMuYWRvYmUuY29tL3hhcC8xLjAvIj4KICAgICAgICAgPHhtcDpDcmVhdG9yVG9vbD53d3cuaW5rc2NhcGUub3JnPC94bXA6Q3JlYXRvclRvb2w%2BCiAgICAgIDwvcmRmOkRlc2NyaXB0aW9uPgogICA8L3JkZjpSREY%2BCjwveDp4bXBtZXRhPgqyyWIhAAAJG0lEQVRYCaVYW2xcRxn%2B51x2z6693thu7boOCbawGzXFEShSUB%2Bg8MBFFaCqD3koRcADQqoUVIwQD5TygATiASHEQwVIXIqEqj6AUKWipmpIiQopIcJq08Yxie3Yydo4sXe9tnf37LnwfWOPvZvsrtfOL83OnjMz%2F3zn%2B29zjpJ7kHPnzmX6%2BvoeVkod8jyvP5FIHLJt%2B%2BOpVOo0rucsy5qG%2BrcxXtjvNs5%2BF8ZxbOfz%2BT%2Bur68%2FHgSBAJAAoACUpNPpE9QbhqHMzMy%2BirmfB8hwP3tZ%2B1nENQD3dGdn5%2BPZbFbAmgCobGxsSLValVKphPGCXJuZFdtxPjc5NfXl%2Fe6j9rNwcXGR5ryQTCYPkiUKwUVRpJmsVHzJr65KHIu4js37845tHR8eHl7c6377YhDmeg5%2BdpCmNQKzCgBLsbgmK4VNl1N4%2FAAPAPMf9P3q983cvfR7ZnBhYeEEgJzFJkn6GxsFoLWZl1fyorbu6YGtMduyKlEYfGJ0dPS8ud9OvycGGRhoP4LPJdHX6fd9X9bW1kRJ%2FX1O4lz4YjKM5MdnzpzZU2DuCWAulzsJ9j5ZLpfrwDEwKpWKjloy2Qgkxz0v%2BdjAwMGTdYt3uWgb4NTUVBec%2FXkGQq0QHH2RPZnaBMgZdzPJtbHEz1NXrY5W%2F9sGiKj9KgJjlECMEBgbgbGHn2mfpF%2Bqu%2FFJFXPS6dQI4uZrRsdufVsA%2BcQAcYp%2BRjAUpheTYthr9ixbrCgUK46QG6Ea%2FZ1CkEB%2Fql0W2wIIRp5CqhgmSxSCMUB5TVZpWkl60nX2ZTnw2u9EeWmxcK92HudSBywxBHN%2Fide7ya4AJyYmOqDkmwac9jGC2RLtVwBsUo539T%2BSfufvYsHbWGGAUGKFbdi2hLrgracmFhaou6XsrGoyDeXsJNh7iCwRhN60Zi43M6BVGIiT%2F584t26IXVoTy3XFrZYk%2B9afxb19U2K4AIVmps5ksbhrRLcEOD097cFEz9LHCMzFhkZoOt6PsKlKJHVytisbYhdvi%2BWjLxWR%2B1xxwLZdWhVVXsfSHeZDZoMwfpZ7GJ2N%2BpYAYb4n8aSPEAxPKkZ4TdNGMFsSJk29e05UMi1ufhEAlxEDCBIAog%2Bqrm4pfOopKfcfFhXtlEZaJJnyHqkEwZNGb6O%2BKcBLly4lAORbNKsBtw2M%2BUz7lpKuV16Q7hd%2FoDf3Jv8lCiaF12kTC9hl6rHxcAoWQKmrw6B1RPE496obqLloChCR9gUcAD5KJZRacJo93LervtgbebHBXPdvvycdb%2FxBYi%2BlK4mzuqRBWY4j3npevJUFBgYyz07qYdpCZfoI3OeLNZjq%2FjYEiBzFg8C4A%2BV3AjPXynbEXl%2FRPhcnU5I%2B%2F4rYhSWcr5IC2sTJXUNQIS9iXtebL8mBf%2FxJvOUbcLu7M7hS1jj3rEO2ddEQIPzuY2ysnwYQn5z%2Fw61Ka7sJcRdnRCFaET0Sp9ICXxDB%2BU84lpvSMxnZ3pW3xS3eksy1i2KFVQTXjqnJou26J8JQPdo2QDjwAlrJgDMmjWBS792z4lbWRRHE9feQYsAImN5uYCxGwuaYm7sqCfT20nVxV26KC3dI3Z6XCEuMqZmikA3CyI4bvrc0ZHBoaGgKoN5haiG4TeYsbPhfSV05L6mlGUQoCLt6ET80KQCaRrAAr4KyZH%2F9bcm8%2BJzA1uIWkB%2FLawA4Bw9QqCibNZ1BCLw3rDCcaptBPBVwRW9wMQFutlCc5RyCoiAdf%2Fm5ZH41Ls78Zc3WJnswLc1rGoBbAGUhQPgQCjXahQ%2B6lTVx8J9CkA5zaxy%2Ff%2BTIkWIjgE0PjwD1Onzwu2SPDT%2BimIALi2KBBevWdYkdZoed5MsNmGK2hWwaAXA7nxOHACWSAEk8gP9xNaz8bzPtzr5GQ%2F0Q3tIuIgXkEMkD8BBdShVMw0NAmL1f6PxIbPWLWl3hAVl1dFXSFQbuAwtVShtcdaHZ0qYAx8bGViYnJ%2F8JP3yCSmNlS5zpFTU8JtHcpMQoa1KtbAIlw%2FVE1u%2BnSY1lpfuwhOmsWEhLNuoxSyesVMQee2eQO8C0p%2BGPT2zv1vOASDEn8oGHJF6aA0CcD6tlBITfgk2wjtK31nG%2FlNI9YqWy4iDK7bgE13V4%2FLpw9OhRKGssTRnkdDD3Jt4%2FfARLgj5o9R2WOHcZ9RXBAMeJ11YAELmSTAKsIMdxnpYtRhWuK4kOKXY%2BALYj8fuGhEcOZggCBIuv4rLGcTeXm9%2BWAPGF4Eomk7kMZWM6WDqyEgCkc3NSVPY%2BnU4EpjYgySg8X0es2TOyHFnt6JcYJg37PyhB94BEjF6AQ64N0L9mwDTqG%2BZBM%2FH48eNMVmfpKzqh4gRSHnxYwmQHiEJN7YQ%2FdcFsXb0iuvWg75EY92MPZ1G09QMPCs7bEnXdJ9XRE6CKOTDQL%2FnQ%2Bf7y8vJ7Zr9GfUuAXIB0c9ocSqt%2BRXzHk9LoozhqYRBsxQkcDsAsgSqCzACkbt1SyfZLWRC5nd3iH30MOROg8WDUx4cme1skNMKm77U0MWdA0XmY4jZqc6%2BO5kpZYjh79OHPijeNGotTCyuHlfAAFg3mo8lpxkqoxM%2F2in%2FoGN5XUjqQtCWQFVjn8Z%2F%2B11JaJYfthThp%2FBW%2B%2BBkWdgo34WmZjp4oLqLOokJIGOGAqi0Sw%2B82cKfgdsmGS9YQAyZ4sJ7%2Bh6PcHA%2FDvb29q1ppk59dGeQ6AOEHyW2ADJiAKSZibuyH4w9KYFlRRzoVYa5TqQZSwWcQnx%2BR4BZ3Cs6aZPBvg4ODLcFx3a4%2ByEkw7e%2Fx3eV1KqbvGKHJAwAIYHbfrzgbpXIUIXq0r6JChIzqGiFz1IHscAEAf1gz1PRvWybmarDmzs%2FPfwU58TsogR%2BiDxEgRZt8M6%2FFeAskAFUAe5xD4aEDa%2FAQ%2Fjzm%2FhTfEn957NgxvkXtKm0DNJrm5uZ6sMk30J7Bpg%2FWAqVPAmAMAGoVHzApBIY5tzH%2FBQTbL1A1Foyudvo9AzRKUacHETjPgNmvA0Qvv3jxWAYTxvivyC4Y4xHqN7j%2FMxynps3avfT7Bmg2mZ2dHQZz4zDj03j7y5BFmLcM1l7CvZ%2BMjIy0TMRGT7P%2BngEaxTD9CILg02B0AAy%2BjO%2FRE2bsXvr%2FAx2ptl%2F0braeAAAAAElFTkSuQmCC)](https://firecracker-microvm.github.io)
[![中文](https://img.shields.io/badge/LANG-%E4%B8%AD%E6%96%87-DA3633?style=flat-square&labelColor=161B22)](README-zh.md)

</div>

**A sandbox fleet that is oversold on purpose.** Most agent sandboxes sit idle most of the time. Sandstorm pauses an idle sandbox, then checkpoints it to disk, and brings it back the moment a request reaches it. So a machine holds far more sandboxes than it could keep running. Every sandbox is a [gVisor](https://gvisor.dev) container with its own network namespace, and Sandstorm runs on ordinary cloud VMs: no KVM, no nested virtualisation. On a machine with KVM a sandbox can be a [Firecracker](https://firecracker-microvm.github.io) microVM instead (`--backend firecracker`), which started faster than every other sandbox we measured ([below](#on-a-kvm-machine-against-the-other-sandboxes)).

On six cloud machines with 2 cores and 2 GB each, which hold **286** Python HTTP servers kept in memory, Sandstorm held **20,000** — and of 1,200 picked at random afterwards and called, 1,198 answered.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-overview-dark.svg">
  <img alt="Four panels. On the left, capacity: 20,000 sandboxes on six 2-core, 2 GB machines that hold 286 kept in memory, and by heap size, how many a node keeps in memory as a sandbox and as a plain process, with the wake time from disk under each size. On the right, latency: by idle time, and under rising traffic against what round 3 refused" src="docs/assets/charts/fleet-overview-light.svg" width="800">
</picture>

</div>

The master serves a console at `/`: where every sandbox is, how fast they move between tiers, and each node's headroom. Here on a simulated six-node fleet.

<p align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/console-dark.webp">
  <img alt="The Sandstorm console: counts of sandboxes, awake ones, nodes, creates per minute and API p99 across the top; the tier flow between awake, frozen, on disk and detached; sandboxes by tier over time, lifecycle rates, node headroom and latency." src="docs/assets/console-light.webp">
</picture>
</p>

## How it works

Every sandbox is in one of four tiers. It drops a tier at a time as it stays idle, and any request takes it straight back to hot.

| Tier | Where it is | Holds memory | Back to hot |
|---|---|---|---|
| **hot** | running | yes | — |
| **frozen** | paused in place (`cgroup.freeze`); its cold pages squeezed into zswap | some | ~10 ms |
| **snapshot** | checkpointed to the node's disk; its network namespace released | no | ~0.2 s |
| **detached** | only its volume is left | no | cold start |

- **One master, many nodes.** The master keeps desired state in SQLite and decides placement. The master connects to a node by address and pairing code, or a node behind NAT dials the master.
- **Wake on traffic.** A request to a sandbox's port reaches a gateway on its node, which wakes the sandbox and then forwards the request.
- **Nodes protect themselves.** A node short on memory turns work away instead of running out, and still answers ssh.
- **One binary.** `sandstormd` is the master, the node and the web console.

## Latency and cost

The other three panels of the figure at the top, the two on the right first:

- **Waking.** Timed on the node, so no network is in the path. A hot sandbox answers in 3.8 ms. Frozen, the thaw takes 11 ms and reading its heap back from zswap takes most of the rest: about 100 ms. On disk, the restore is about 230 ms for a 64 MiB heap.
- **Under traffic.** 40 sandboxes per node, where about 12 fit awake, with a few of them getting most requests. A wake that finds the node out of memory puts the sandboxes least in demand to disk to make room, and one that cannot start within 2 s is refused rather than queued, so at 1, 2, 4 and 8 requests a second per node **1.1%, 0.1%, 0.2% and 2.5%** of requests were refused, against 13%, 26%, 32% and 33% before (three runs, 40,500 requests). The cost is in the tail: a request that has to wait for a restore takes about 2 s at p99, at every rate. Past what a node can turn over — 80 per node at 8 req/s — it refuses 10.6%, with a p99 of 3.2 s.
- **Heavier sandboxes.** With a large heap, a sandbox costs about what the same program does as a plain process: 11 against 10 per node at 64 MiB, 4 against 3 at 256 MiB, 2 and 1 for both at 512 MiB and 1 GiB, because the node squeezes idle pages into zswap and the test heap compresses well. With a small one, gVisor's own overhead dominates, 19 against 24 at 16 MiB, which is what tiering makes up for. Waking from disk grows with the heap: 0.2 s at 64 MiB, 1 s at 256 and 512 MiB, 3.6 s at 1 GiB.

## Against the same machines without Sandstorm

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/fleet-compare-dark.svg">
  <img alt="Python HTTP servers one 2-core, 2 GB node holds: about 96 as plain processes, 94 under bubblewrap, 88 under Podman, 60 under Docker, 32 under Docker with gVisor, 48 in Sandstorm with every one awake, and 3,333 in Sandstorm with idle ones tiered" src="docs/assets/charts/fleet-compare-light.svg" width="800">
</picture>

</div>

The same Python HTTP server on the same six nodes, each runtime kept in memory until 512 MiB is left, twice on every node ([`baselines.py`](bench/fleet/baselines.py)). Sandstorm with every sandbox awake holds half again as many as Docker with gVisor, the same isolation, because it runs no daemon and no shim per container; with idle ones tiered it holds two orders of magnitude more than anything that keeps them in memory.

| Per node, 2 vCPU, 1.9 GB | HTTP server kept in memory | 64 MiB heap |
|---|---:|---:|
| plain process | 96 | 11 |
| bubblewrap | 94 | 11 |
| Podman (crun) | 88 | 12 |
| Docker (runc) | 60 | 10 |
| Docker + gVisor | 32 | 9 |
| **Sandstorm, all awake** | **48** | **13** |
| **Sandstorm, idle ones tiered** | **3,333** | |

Starting, timed on one machine (bm: AMD EPYC 9K65, 2 vCPU, 3.6 GB; dev at 872682b) the way the baselines are: one at a time, polled every 50 ms until it answers ([`starttime.py`](bench/fleet/starttime.py)). Sandstorm's time includes the master placing the sandbox and the node setting up its network.

| Start to first answer, p50 | web | 64 MiB app |
|---|---:|---:|
| plain process | 52 ms | 103 ms |
| bubblewrap | 52 ms | 103 ms |
| Docker (runc) | 192 ms | 244 ms |
| Docker + gVisor | 217 ms | 333 ms |
| **Sandstorm** | **167 ms** | **289 ms** |
| **Sandstorm, warm template** | **159 ms** | **168 ms** |

Of Sandstorm's 289 ms for the 64 MiB app, 37 ms is the master and the network, 5 ms the bundle, 55 ms runsc and 189 ms Python importing numpy and filling its heap. A warm template restores an image of the app already started, so it skips that last part; it is opt-in, because every sandbox starts from the same memory and the template has to say how to reseed. The 50 ms poll is the floor.

Checkpoint and restore with CRIU under Podman, the nearest standard counterpart to the snapshot tier, takes 0.3 s to checkpoint a 64 MiB app and 0.24 s from restore to first answer; Sandstorm restores the same app in 0.2 s. E2B, CubeSandbox and microsandbox are built on Firecracker and need KVM, which these VMs do not have; they are measured on a machine that has it below, and [docs/COMPARISON.md](docs/COMPARISON.md) covers the field.

## On a KVM machine, against the other sandboxes

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/baremetal-start-dark.svg">
  <img alt="The 64 MiB app on one KVM machine, start to first answer and memory per instance: Sandstorm on Firecracker from a warm template 74 ms and 10 MiB, CubeSandbox 78 ms and 12 MiB, Sandstorm on gVisor from a warm template 123 ms and 97 MiB, Sandstorm on gVisor 545 ms and 98 MiB, Docker 414 ms and 88 MiB, Docker with gVisor 660 ms and 107 MiB, microsandbox 1.1 s and 166 MiB, Kata 3.4 s and 310 MiB" src="docs/assets/charts/baremetal-start-light.svg" width="800">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/baremetal-clone-dark.svg">
  <img alt="Cloning a running 64 MiB app into 1, 10 and 20 copies that answer: Sandstorm on Firecracker 137, 528 and 980 ms; CubeSandbox 542, 973 and 1,046 ms; microsandbox 658, 835 and 1,152 ms to running; Sandstorm on gVisor 458, 1,340 and 2,280 ms" src="docs/assets/charts/baremetal-clone-light.svg" width="800">
</picture>

</div>

The runtimes that need KVM, run on one machine that has it: 2 × Xeon E5-2637 v2, 31 GB, Ubuntu 24.04. Every runtime in turn, the same two programs and the same harness ([`compare.py`](bench/baremetal/compare.py)). There Sandstorm can also run each sandbox as a [Firecracker](https://firecracker-microvm.github.io) microVM (`--backend firecracker`), and from a warm template that is the fastest start measured, in about CubeSandbox's memory.

| p50 | web | 64 MiB app | memory, web / 64 MiB app |
|---|---:|---:|---:|
| **Sandstorm, Firecracker, warm template** | **62 ms** | **74 ms** | 9.9 / **10.5 MiB** |
| CubeSandbox 0.7.3 | 72 ms | 78 ms | **8.9** / 12.2 MiB |
| **Sandstorm, gVisor, warm template** | 107 ms | 123 ms | 23.3 / 97.3 MiB |
| **Sandstorm, gVisor** | 234 ms | 545 ms | 24.1 / 97.9 MiB |
| Docker + gVisor | 327 ms | 660 ms | 30.8 / 106.8 MiB |
| microsandbox 0.7.8 | 1,117 ms | 1,119 ms | 80.2 / 166.2 MiB |
| Kata 4.2 (QEMU) | 3,005 ms | 3,397 ms | 223 / 311 MiB |

Sandstorm on Firecracker snapshots a running sandbox in 27 ms, against CubeSandbox's 252, and a clone of one into n is that snapshot and one fork call. Its children are created one after another on the node, about 50 ms each, where CubeSandbox and microsandbox create theirs at once, so a larger n would close the gap. E2B self-hosted was not set up. Method, raw rows and what the runs turned up: [bench/baremetal/results/2026-10-30](bench/baremetal/results/2026-10-30/README.md).

## Numbers

Six nodes (AMD EPYC, 2 vCPU, 1.9 GB each) and one master (2 vCPU), runsc `release-20261019.0`, on 2026-10-28.

| | |
|---|---|
| 20,000 Python HTTP servers at 20/s | all 20,000 up with nothing retried by hand, twice |
| 100 of them per node, picked at random, called | 1,198 of 1,200 answered; p50 1.3 s, p99 2.8–5.5 s |
| Wake: frozen / from disk, 64 MiB app | 11 ms / 200 ms (Sandstorm's own time) |
| Wake 20 / 40 at once on one node | all of them; slowest 6 s / 13 s |
| Create to first answer, 64 MiB app | p50 1.4 s with five starting at once on a node, polled every 0.25 s; 0.29 s one at a time (above) |

Some caveats. The idle timers were shortened so a run fits in minutes. The test heap is `numpy.ones`, which compresses unusually well in zswap. 20,000 is where the fill stopped, not a ceiling. Method, raw data, every run, and the bugs the runs turned up are in [bench/fleet/results/2026-10-28](bench/fleet/results/2026-10-28/README.md); the earlier round is [2026-10-26](bench/fleet/results/2026-10-26/README.md).

## Quick start

You need Linux with cgroup v2, [`runsc`](https://gvisor.dev/docs/user_guide/install/) on the `PATH`, and Rust to build.

```bash
git clone https://github.com/deeplethe/sandstorm.git && cd sandstorm
cargo build --release -p sandstorm-core
sudo ./target/release/sandstormd serve --master --node
```

That runs a master and a node in one process, with the console at `https://127.0.0.1:8080`. The API token is in `/var/lib/sandstorm/pki/api-token`. A sandbox starts from a template, a rootfs plus an OCI spec registered with `POST /v1/templates`; `scripts/fleet.sh seed` shows the smallest one.

**A fleet** of machines you can ssh to:

```bash
scripts/fleet.sh up --master m1 --nodes n1,n2,n3 --listen 80
scripts/fleet.sh seed     # put a template on each node
scripts/fleet.sh verify   # start one sandbox and check it runs
```

**A node that is already installed** listens with `SANDSTORM_LISTEN=0.0.0.0:80`. On first start it writes a pairing code to `/var/lib/sandstorm/pki/node/pairing-code`. Give the master its address and that code, with the master's CA and API token from its `/var/lib/sandstorm/pki`:

```bash
curl --cacert ca.pem -H "Authorization: Bearer $(cat api-token)" -H 'Content-Type: application/json' \
  https://master:8080/v1/nodes -d '{"address": "10.0.0.5", "pairing_code": "<code>"}'
```

The code stays the same across restarts, and whoever holds it can run anything on that node. Keep it like a root password.

The HTTP API is documented in [docs/API.md](docs/API.md). To load-test a fleet, use [bench/fleet](bench/fleet/README.md).

## Status

v0.6 is not released yet, and it changes a lot: the backend moves from Firecracker microVMs to gVisor, and one host becomes a fleet. The last release, v0.5.3, is the Firecracker fork runtime; its README is [at that tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3). The design decisions are recorded in [docs/adr](docs/adr/README.md).

Next, from what the fleet runs found:

- [x] Make room on wake: put the sandbox least in demand to disk instead of refusing the request.
- [x] Retry a create the node held back once memory frees, instead of parking it; and clear its error once it runs.
- [x] Clear away a sandbox that dies, network included, and restart it if it is still wanted.
- [ ] Reserve capacity on the master as it places, so a burst cannot overshoot a node's last report.
- [ ] Restore several snapshots in parallel on one node.

## License

[Apache-2.0](LICENSE)
