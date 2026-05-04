# Cross-platform validation report — v0.6.3.1 / issue #487

**Author:** PR-9 Audit Agent D
**Date:** 2026-04-30
**Branch:** `release/v0.6.3.1-issue-487-pr9d-cross-platform-validation`
**Scope:** Validate the session-boot lifetime suite (PR-3) on platforms NOT
covered by the GitHub Actions matrix for `session-boot-lifetime.yml` —
specifically Kubernetes (kind cluster) and ARM Linux (cross-compiled).

This is a **best-effort, host-honest** report. Where the host did not have the
required tooling, this document captures a reproducible recipe so a future
runner can execute it without re-deriving the steps.

---

## tl;dr

| Platform | Tested? | Result |
|---|---|---|
| Kubernetes (kind, single node, sidecar pod with PVC) | **skipped — recipe only** | host has `docker` CLI but no Docker daemon running and no `kind` binary; both are host-modification steps |
| ARM Linux (`aarch64-unknown-linux-gnu`, cross-compiled from this macOS box) | **skipped — recipe only** | host has only `aarch64-apple-darwin` target installed; adding `aarch64-unknown-linux-gnu` requires `rustup target add` plus a cross linker (host-mod) |
| ARM Linux (native, on `ubuntu-24.04-arm` runner) | **partially — release build path only, NOT the lifetime suite** | `ci.yml` already builds `aarch64-unknown-linux-gnu` on `ubuntu-24.04-arm` for release artifacts; that job runs `cargo build --release` but does NOT invoke the PR-3 `cargo test --test boot_*` targets |

**Net:** zero new validation runs were executed by this PR. What this PR does
deliver is (a) a precise statement of which platforms remain unexercised, and
(b) drop-in YAML / shell recipes for the next runner to execute under
maintainer authorization.

---

## 1. Existing CI matrix — what's already covered

For reference, here is what the merged PRs already exercise:

### `.github/workflows/session-boot-lifetime.yml` (PR-3)

```yaml
matrix:
  os: [ubuntu-latest, macos-latest, windows-latest]
```

- `ubuntu-latest` — x86_64 GNU/Linux
- `macos-latest` — arm64 (GitHub switched the default to arm64 mid-2024)
- `windows-latest` — x86_64 Windows MSVC

The suite invocation:

```bash
cargo test --test boot_primitive_contract --test recipe_contract --test boot_lifecycle
```

### `.github/workflows/ci.yml` (release matrix)

```yaml
include:
  - target: x86_64-unknown-linux-gnu       # ubuntu-latest
  - target: aarch64-unknown-linux-gnu      # ubuntu-24.04-arm  ← native ARM runner
  - target: x86_64-apple-darwin            # macos-latest
  - target: aarch64-apple-darwin           # macos-latest
  - target: x86_64-pc-windows-msvc         # windows-latest
```

The release matrix runs `cargo build --release` and packages tarballs / .deb /
.rpm — but it does **not** run the lifetime suite. So `aarch64-unknown-linux-gnu`
gets a cleanly compiled binary every release, and the lifetime suite has never
asserted that binary's behavior on Linux/ARM.

### Gap summary

| Target triple | Built? | Lifetime suite run? |
|---|---|---|
| `x86_64-unknown-linux-gnu` | yes (release + lifetime suite) | yes |
| `aarch64-unknown-linux-gnu` | yes (release only) | **no** |
| `x86_64-apple-darwin` | yes (release only) | **no** (`macos-latest` is arm64 since 2024) |
| `aarch64-apple-darwin` | yes (release + lifetime suite) | yes |
| `x86_64-pc-windows-msvc` | yes (release + lifetime suite) | yes |
| Kubernetes runtime (any arch) | n/a | **no** |

---

## 2. Kubernetes validation via kind — recipe (NOT executed)

### Why skipped

```text
$ which kind
kind not found

$ docker info
... failed to connect to the docker API at unix:///var/run/docker.sock
```

The host has the Docker CLI (`/opt/homebrew/bin/docker`) but no running daemon,
and no `kind` binary. Installing kind, starting Docker Desktop, and bootstrapping
a cluster all count as host modification under the audit's standing
constraints. The recipe below is the work product instead.

### Prerequisites (one-time host setup the operator must authorize)

```bash
# macOS (Homebrew)
brew install kind kubectl
open -a Docker        # start Docker Desktop and wait until `docker info` succeeds

# Linux
go install sigs.k8s.io/kind@latest
# kubectl from your distro or https://kubernetes.io/docs/tasks/tools/
sudo systemctl start docker
```

### Step 1 — bring up a single-node kind cluster

```bash
kind create cluster --name ai-memory-pr9d --wait 60s
kubectl config use-context kind-ai-memory-pr9d
kubectl get nodes -o wide
```

### Step 2 — load the locally built ai-memory image into the cluster

We use the existing top-level `Dockerfile` (multi-stage; runtime is
`debian:bookworm-slim`).

```bash
# From the repo root
docker build -t ai-memory:pr9d-validation .
kind load docker-image ai-memory:pr9d-validation --name ai-memory-pr9d
```

### Step 3 — apply the validation manifests

The two manifests below are exactly what the operator should `kubectl apply -f`.
They are pinned to the validation image tag built in step 2.

#### `manifests/ai-memory-pvc.yaml`

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: ai-memory-data
  namespace: default
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
```

#### `manifests/ai-memory-sidecar.yaml`

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: ai-memory-sidecar-validation
  namespace: default
  labels:
    app: ai-memory
    audit: pr9d
spec:
  restartPolicy: Never
  volumes:
    - name: db
      persistentVolumeClaim:
        claimName: ai-memory-data
  containers:
    # Sidecar — long-running ai-memory binary the agent will exec into.
    # We don't run `serve` here because the validation only needs the
    # binary on PATH inside a pod sharing the PVC; `sleep infinity`
    # keeps the container alive long enough to be exec'd.
    - name: ai-memory
      image: ai-memory:pr9d-validation
      imagePullPolicy: IfNotPresent
      command: ["sleep", "infinity"]
      env:
        - name: AI_MEMORY_DB
          value: /data/ai-memory.db
        - name: AI_MEMORY_NO_CONFIG
          value: "1"
      volumeMounts:
        - { name: db, mountPath: /data }
    # Agent — minimal ubuntu image that simulates an AI agent calling
    # `ai-memory boot` on session start. We exec from outside the pod
    # for the assertion (see step 4) so we capture stdout deterministically.
    - name: agent
      image: ubuntu:24.04
      command: ["sleep", "infinity"]
      volumeMounts:
        - { name: db, mountPath: /data }
```

```bash
kubectl apply -f manifests/ai-memory-pvc.yaml
kubectl apply -f manifests/ai-memory-sidecar.yaml
kubectl wait --for=condition=Ready pod/ai-memory-sidecar-validation --timeout=60s
```

### Step 4 — seed and assert

```bash
# Seed two memories via the sidecar's CLI (writes to the shared PVC)
kubectl exec ai-memory-sidecar-validation -c ai-memory -- \
    ai-memory --json store -n k8s-validation -T first  -c "content one"
kubectl exec ai-memory-sidecar-validation -c ai-memory -- \
    ai-memory --json store -n k8s-validation -T second -c "content two"

# Run boot the way an agent would on session start
kubectl exec ai-memory-sidecar-validation -c ai-memory -- \
    ai-memory boot --namespace k8s-validation --limit 10 \
    | tee /tmp/k8s-boot-stdout.txt

# Assert: the boot manifest header must lead with the ok status line.
grep -q '^# ai-memory boot: ok' /tmp/k8s-boot-stdout.txt && echo PASS || { echo FAIL; exit 1; }

# Assert: both seeded titles appear in the manifest.
grep -q 'first'  /tmp/k8s-boot-stdout.txt || { echo "missing 'first' title"; exit 1; }
grep -q 'second' /tmp/k8s-boot-stdout.txt || { echo "missing 'second' title"; exit 1; }
echo "k8s validation PASS"
```

### Step 5 — teardown

```bash
kubectl delete pod ai-memory-sidecar-validation
kubectl delete pvc ai-memory-data
kind delete cluster --name ai-memory-pr9d
```

### What this recipe proves (when executed)

1. `ai-memory` runs unmodified inside a pod sharing a PVC — i.e. the binary
   does not assume host-local filesystem semantics that fail on networked
   block storage.
2. `ai-memory boot` returns the same manifest shape (ok header + memory list)
   inside Kubernetes that PR-3 already asserts on bare ubuntu/macos/windows
   runners.
3. The sidecar pattern in the manifest is a working reference for users who
   want to deploy ai-memory alongside an agent workload; today the only
   reference deployment story is the systemd unit in `packaging/systemd/`.

---

## 3. ARM Linux cross-compile — recipe (NOT executed)

### Why skipped

```text
$ rustup target list --installed
aarch64-apple-darwin
```

The only installed target is `aarch64-apple-darwin` (this is an Apple Silicon
Mac). Adding `aarch64-unknown-linux-gnu` requires `rustup target add`, which is
host modification.

### Prerequisites (one-time host setup the operator must authorize)

```bash
# Add the Rust target
rustup target add aarch64-unknown-linux-gnu

# Install a cross linker (macOS host)
brew tap messense/macos-cross-toolchains
brew install aarch64-unknown-linux-gnu

# Tell cargo about it (project-local — this stays in the worktree, no global config)
cat >> .cargo/config.toml <<'EOF'

[target.aarch64-unknown-linux-gnu]
linker = "aarch64-unknown-linux-gnu-gcc"
EOF
```

> Note: the operator may already have a different cross toolchain (e.g. the
> `cross` cargo subcommand backed by Docker). If so, `cross build --release
> --target aarch64-unknown-linux-gnu` is the one-line equivalent and avoids
> the linker-config step entirely. `cross` itself, however, requires the
> Docker daemon — same blocker as the kind path above.

### Step 1 — build

```bash
cargo build --release --target aarch64-unknown-linux-gnu
```

### Step 2 — assert the binary is ARM64 ELF

```bash
file target/aarch64-unknown-linux-gnu/release/ai-memory
# Expected output (substring match):
#   ELF 64-bit LSB ... ARM aarch64 ...
```

```bash
file target/aarch64-unknown-linux-gnu/release/ai-memory \
    | grep -E 'ELF 64-bit.*aarch64' \
    && echo "ARM64 build PASS" \
    || { echo "FAIL: not an aarch64 ELF"; exit 1; }
```

### Step 3 (optional) — exercise the lifetime suite under qemu-user

If the operator wants to actually run the suite (not just build), the cleanest
path on macOS is `qemu-user-static` via Docker:

```bash
docker run --rm --platform linux/arm64 \
    -v "$PWD":/work -w /work \
    rust:1.94-slim-bookworm \
    bash -c 'apt-get update && apt-get install -y pkg-config libssl-dev build-essential \
             && AI_MEMORY_NO_CONFIG=1 cargo test \
                --test boot_primitive_contract \
                --test recipe_contract \
                --test boot_lifecycle'
```

This is functionally what a real `ubuntu-24.04-arm` GitHub runner would do; on
a Mac host it's the same `qemu-user` emulation that any container-based ARM CI
provides. Same Docker daemon prerequisite as the kind path.

### Cleanest CI fix (preferred to host-side validation)

The simplest closure is to extend the lifetime suite matrix in
`.github/workflows/session-boot-lifetime.yml`:

```yaml
matrix:
  os: [ubuntu-latest, ubuntu-24.04-arm, macos-latest, windows-latest]
```

`ubuntu-24.04-arm` is the same runner already used by the release matrix in
`ci.yml`, so the runner image is known-good. This change is a one-line PR and
would close the ARM Linux gap without needing any host-side reproduction.
**Filing this as a follow-up rather than including the workflow edit here**
because the audit charter is "documentation only — file an issue if a bug
surfaces". It's not a bug, but it is an obvious next step.

---

## 4. What was actually exercised vs. documented

| Item | Status |
|---|---|
| Repo on `release/v0.6.3.1` | **confirmed** — `git log` shows all 8 PRs merged through commit `d974112` |
| Lifetime suite source files exist as expected (`tests/boot_primitive_contract.rs`, `tests/boot_lifecycle.rs`, `scripts/run-session-boot-lifetime-tests.sh`) | **confirmed by inspection** |
| CI matrix gap analysis | **executed** — see §1 |
| `which kind`, `which docker`, `docker info`, `rustup target list --installed` | **executed** — see §2 / §3 "why skipped" |
| kind cluster bring-up + sidecar pod + boot assertion | **NOT executed** — recipe in §2 |
| `cargo build --release --target aarch64-unknown-linux-gnu` | **NOT executed** — recipe in §3 |
| `file <binary> | grep aarch64` assertion | **NOT executed** — recipe in §3 step 2 |
| qemu-user lifetime-suite run | **NOT executed** — recipe in §3 step 3 |

---

## 5. Known gaps (documented but not exercised)

These are the platforms / scenarios where this audit produced a recipe but no
green run. They remain open until a future runner with the right host
authorization executes the recipes above:

1. **Kubernetes runtime (kind, single node)** — the sidecar+PVC pattern in §2
   is unverified. Risk surface: any code path that assumes a host-local
   filesystem (e.g. SQLite WAL behavior on a network-attached PVC) could fail
   silently in production k8s deployments and the lifetime suite would not
   catch it today.
2. **`aarch64-unknown-linux-gnu` lifetime suite** — release builds happen on
   `ubuntu-24.04-arm` for tarball / .deb / .rpm packaging but `cargo test
   --test boot_*` is never invoked. Risk surface: ARM64 Linux is the default
   architecture for AWS Graviton, GCP Tau T2A, Oracle Ampere — all common
   production targets. A regression specific to ARM Linux (e.g. an unaligned-
   access bug in a transitive C dep) would ship.
3. **`x86_64-apple-darwin` lifetime suite** — `macos-latest` has been arm64
   since mid-2024, so the macOS lifetime job no longer covers Intel Macs.
   Less urgent because Apple is sunsetting Intel, but worth noting for any
   user still on a 2019-era MacBook Pro.
4. **Other Kubernetes flavors** — the recipe is kind-specific. EKS, GKE, AKS,
   OpenShift, k3s, and Talos all have subtle differences (storage class
   defaults, security context constraints, etc.) that this audit makes no
   claim about.
5. **musl Linux (`*-unknown-linux-musl`)** — Alpine-based container deployments
   are common; not in the release matrix and not covered here. Out of scope
   for this audit but flagging for completeness.

---

## 6. Tooling installs needed for full closure

If the maintainer wants to authorize the next iteration to actually run the
recipes above, here is the exact set of host changes required:

### For kind validation (§2)

```bash
brew install kind kubectl     # macOS, or the Linux equivalents
open -a Docker                # macOS — start Docker Desktop
# wait until `docker info` returns 0
```

Risk: low. Adds three CLI tools and starts a desktop daemon. Reversible via
`brew uninstall` and quitting Docker.

### For ARM Linux cross-compile (§3)

```bash
rustup target add aarch64-unknown-linux-gnu
brew tap messense/macos-cross-toolchains
brew install aarch64-unknown-linux-gnu
# plus the .cargo/config.toml linker entry shown in §3
```

Risk: low. Reversible via `rustup target remove` and `brew uninstall`.

### For lifetime suite under qemu (optional, §3 step 3)

Same Docker daemon as the kind path. No additional installs (the
`rust:1.94-slim-bookworm` image is pulled at run time and cleaned up with
`--rm`).

### Cleanest alternative — extend CI matrix instead

```diff
 matrix:
-  os: [ubuntu-latest, macos-latest, windows-latest]
+  os: [ubuntu-latest, ubuntu-24.04-arm, macos-latest, windows-latest]
```

Zero host changes. One-line workflow edit. This is the recommended path; the
host-side recipes in §2 and §3 are documented as a fallback for operators who
want to reproduce a CI failure locally rather than as the primary closure
mechanism.

---

## 7. Audit charter compliance

- **No host modifications performed.** Only read-only commands ran (`which`,
  `docker version`, `rustup target list --installed`, `git log`, `ls`, file
  inspection).
- **No code changes.** This is a pure-docs PR. No bug was surfaced in the
  lifetime suite during this audit; if the recipes here are executed and a
  regression is found, file a separate issue per the PR-9d charter.
- **Honest about what's a recipe vs. what's a green run.** Every "recipe"
  block above is explicitly labeled as not executed. The §4 status table is
  the single source of truth.

---

## AI involvement

- **Author:** Claude Opus 4.7 (1M context), running as PR-9 Audit Agent D
  under the issue #487 audit charter.
- **Authority class:** Trivial (documentation-only, no code, no destructive
  ops). Per `docs/AI_DEVELOPER_GOVERNANCE.md`.
- **Reviewer:** maintainer (human) — required before merge per audit policy.
