# `python-numpy`

The canonical sandstorm parent — Python 3.12 with numpy preinstalled.
This is the image our front-page benchmark chart measures.

## What you get

- `python:3.12-slim` base
- `python3-numpy` from apt (~30 MB, includes BLAS via OpenBLAS)
- sandstorm-init.sh + sandstorm-agent.py baked in as PID 1

Total rootfs: **~1.5 GB**, memory image after warm-up: **~512 MiB**.

## Use it

```bash
sudo bash recipes/python-numpy/build.sh
sudo bash scripts/host-tap.sh                # one-time host tap setup
sudo sandstorm snapshot --tag pyagent \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/python-numpy/parent.ext4 \
    --tap sandstorm-tap0

# Fork 100 sandboxes; each can `eval()` numpy expressions instantly.
sudo bash scripts/netns-setup.sh 100
sudo -E sandstorm fork --tag pyagent -n 100 --per-child-netns
sudo sandstorm eval --child sandstorm-child-1 -- "numpy.zeros(5).tolist()"
# [0.0, 0.0, 0.0, 0.0, 0.0]
```

## When to pick this

- You want to **reproduce the benchmark numbers**.
- Your workload uses NumPy / SciPy but doesn't need a full ML stack.
- You want the smallest possible Python image with one heavy import
  already warmed.

For heavier ML (PyTorch, TensorFlow), build a custom rootfs from a
PyTorch base image — see [`coding-agent/`](../coding-agent/) for the
pattern.
