# Hello World WASM Project on K8S

This guide shows how to prepare a MicroK8s cluster (master + workers) to run **WASM/WASI** workloads via **Wasmtime**, how to **build** a small hello world app and compile it to WASM, **package** it as a proper **OCI compatible image**, **push** it with `oras`, and **run** it on Kubernetes.

---

## 0) Prerequisites

- A MicroK8s cluster with **1 master** and **≥1 worker** already joined. You can follow [this gist](https://gist.github.com/gustavojardim/5429eb6b51af3512b77be22278505bd4) for the cluster setup 

---

## 1) Enable & Expose the In‑Cluster Registry (Master)

MicroK8s includes an internal registry. We’ll enable it and expose it via **NodePort 32000** so workers and your dev box can push/pull images.

```bash
# Enable the built‑in registry
microk8s enable registry

# Expose the registry on NodePort 32000 (one‑time)
microk8s kubectl -n container-registry patch svc registry \
  --type=merge -p '{"spec":{"type":"NodePort","ports":[{"port":5000,"nodePort":32000}]}}'
```

> **What this does**: Creates/updates the `container-registry/registry` Service to `NodePort` so it listens on each node at `:32000`. We’ll point `oras` and the nodes to `http://<MASTER_IP>:32000`.

---

## 2) Install the Wasmtime CRI Shim on Each Worker

Wasmtime runs WASM modules. The **containerd Wasmtime shim** (`containerd-shim-wasmtime-v1`) lets Kubernetes launch WASM containers using RuntimeClasses.

### 2.1 Install toolchain & Rust

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev cmake protobuf-compiler git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

> **Why Rust?** The `runwasi` repo (which provides the shim) builds the Wasmtime CRI shim with Rust.

### 2.2 Build and install the Wasmtime shim

```bash
git clone https://github.com/containerd/runwasi.git
cd runwasi

# Build only the Wasmtime shim
cargo build -p containerd-shim-wasmtime --release

# Install where containerd expects it
sudo cp target/release/containerd-shim-wasmtime-v1 /usr/local/bin/ 2>/dev/null || true
if [ ! -f /usr/local/bin/containerd-shim-wasmtime-v1 ]; then
  sudo cp target/release/containerd-shim-wasmtime /usr/local/bin/
  sudo ln -sf /usr/local/bin/containerd-shim-wasmtime /usr/local/bin/containerd-shim-wasmtime-v1
fi
sudo chmod +x /usr/local/bin/containerd-shim-wasmtime*
```

> **Binary name note**: Some builds output `containerd-shim-wasmtime`, others `-v1`. We ensure `-v1` exists by symlink.

### 2.3 Register the runtime in MicroK8s containerd

**Edit on each worker**:

```bash
sudo nano /var/snap/microk8s/current/args/containerd-template.toml
```

Add this block near other runtimes:

```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.wasmtime]
  runtime_type = "io.containerd.wasmtime.v1"
# If your binary isn’t named containerd-shim-wasmtime-v1, also set:
#  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.wasmtime.options]
#    BinaryName = "containerd-shim-wasmtime"
```

Apply the changes by restarting only MicroK8s daemons (not the OS containerd):

```bash
sudo snap restart microk8s.daemon-containerd
sudo snap restart microk8s.daemon-kubelite
microk8s status --wait-ready # this command only runs on the master node
```

### 2.4 Create a `RuntimeClass` (once per cluster)

Create a `runtimeclass-wasmtime.yaml`:

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: wasmtime
handler: wasmtime
```

Apply and verify:

```bash
kubectl apply -f runtimeclass-wasmtime.yaml
kubectl get runtimeclass
```

---

## 3) Allow Workers to Pull from the Registry over Plain HTTP

We’ll configure `containerd` on *each worker* so it treats the master’s registry `http://<MASTER_IP>:32000` as a valid (plain‑HTTP) remote. Replace `REGIP` with your master IP.

```bash
REGIP=192.168.0.117   # master IP exposing NodePort 32000

sudo mkdir -p /var/snap/microk8s/current/args/certs.d/${REGIP}:32000
sudo tee /var/snap/microk8s/current/args/certs.d/${REGIP}:32000/hosts.toml >/dev/null <<EOF
server = "http://${REGIP}:32000"
[host."http://${REGIP}:32000"]
  capabilities = ["pull", "resolve", "push"]
EOF

# (optional) allow localhost:32000 as well
sudo mkdir -p /var/snap/microk8s/current/args/certs.d/localhost:32000
sudo tee /var/snap/microk8s/current/args/certs.d/localhost:32000/hosts.toml >/dev/null <<'EOF'
server = "http://localhost:32000"
[host."http://localhost:32000"]
  capabilities = ["pull", "resolve", "push"]
EOF

sudo snap restart microk8s.daemon-containerd
```

> **Why this is needed**: By default, containerd expects HTTPS registries. This tells each worker it’s okay to use the in‑cluster plain‑HTTP registry for pull/push/resolve operations.

---

## 4) Build a WASI Application on your "client" machine (outside the cluster)

We’ll use a **Rust** example and compile for the `` target (the modern WASI Preview 1 target).

```bash
rustup target add wasm32-wasip1
# inside your Rust project
cargo build --release --target wasm32-wasip1

# produce the module at ./app.wasm (rename as needed)
cp target/wasm32-wasip1/release/<your-binary>.wasm app.wasm
```
---

## 5) Package & Push as an **OCI compat** Image with ORAS

`containerd` expects an **OCI image** with a valid **config** and a single **layer** containing your `.wasm`. We’ll create a tar layer and a `config.json` with required annotations.

From the folder where `app.wasm` exists:

```bash
# 5.1 Create a single-layer tar with app.wasm at tar root
tar -cf hello-wasm.tar app.wasm
LAYER_SHA=$(sha256sum hello-wasm.tar | awk '{print $1}')

# 5.2 Create the OCI config.json
cat > config.json <<EOF
{
  "created": "$(date -u +%FT%TZ)",
  "architecture": "wasm",
  "os": "linux",
  "rootfs": { "type": "layers", "diff_ids": ["sha256:${LAYER_SHA}"] },
  "config": { "Entrypoint": ["/app.wasm"] },
  "annotations": { "module.wasm.image/variant": "compat" }
}
EOF

# 5.3 Push with ORAS
REG=192.168.0.117:32000
NAME=hello-wasm
TAG=latest

oras push --plain-http ${REG}/${NAME}:${TAG} \
  --config config.json:application/vnd.oci.image.config.v1+json \
  hello-wasm.tar:application/vnd.oci.image.layer.v1.tar
```

Verify the push and contents:

```bash
oras repo tags --plain-http ${REG}/${NAME}
oras manifest fetch --plain-http ${REG}/${NAME}:${TAG} | jq .
# (optional) fetch the referenced config blob:
# oras blob fetch --plain-http ${REG}/${NAME}:${TAG} sha256:<config-digest> | jq .
```

### Manifest checklist

- `config.os == "linux"` and `config.architecture == "wasm"`
- `annotations["module.wasm.image/variant"] == "compat"`
- `config.config.Entrypoint` matches the filename at the **tar root** (e.g., `/app.wasm`)
- `rootfs.diff_ids[0]` equals the **uncompressed** tar SHA (we used `LAYER_SHA` from `sha256sum hello-wasm.tar`)

---

## 6) Run on Kubernetes

### 6.1 Minimal Pod

Create `hello-wasm-pod.yaml` (or reuse the existing one in this project):

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: hello-wasm
spec:
  runtimeClassName: wasmtime
  containers:
    - name: hello
      image: 192.168.0.117:32000/hello-wasm:latest
      imagePullPolicy: IfNotPresent
      env:
      - name: WHO
        value: "YOUR NAME"
```

Apply and read logs:

```bash
kubectl apply -f hello-wasm-pod.yaml
kubectl logs -f hello-wasm
```

**Quick one‑liner** (no YAML file):

```bash
kubectl run hello-wasm \
  --image=192.168.0.117:32000/hello-wasm:latest \
  --restart=Never \
  --overrides='{"spec":{"runtimeClassName":"wasmtime"}}'

kubectl logs -f pod/hello-wasm
```
