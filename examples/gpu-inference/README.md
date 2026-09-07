# gpu-inference

Deploy a GPU inference server, with a one-shot weight download and a readiness gate.

## What It Does

1. Runs a one-shot container that downloads the model weights, guarded by `check` so
   it is skipped once the weights are on disk
2. Starts vLLM with the flags GPU workloads actually need — host IPC namespace,
   `--privileged`, `--gpus all`, a large `/dev/shm`, and unlimited locked memory
3. Declares a healthcheck and blocks on `wait "healthy"`, so the next step only runs
   once the API is genuinely serving

## Why `ipc "host"`

The CUDA IPC handles vLLM exports are only mappable from a process in the same IPC
namespace. A KV-cache sidecar (LMCache and friends) running without `ipc "host"`
fails with `cudaErrorMapBufferObjectFailed`.

## Usage

```bash
glidesh run -i examples/gpu-inference/inventory.kdl -p examples/gpu-inference/plan.kdl
```

Set `model`, `model-dir`, and `api-port` in the plan vars for your deployment.
