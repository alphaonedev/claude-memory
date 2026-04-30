# Platform-specific notes

`ai-memory` runs anywhere Rust + SQLite run, which in practice covers every
mainstream agent host. Each platform has its own conventions for binary
paths, config locations, and shell semantics. This doc captures
platform-specific differences for the
[session-boot integration recipes](README.md).

## Platform support matrix

| Platform | Status | Binary location (typical) | Default DB path | Hook scripting |
|---|---|---|---|---|
| **macOS** (Apple Silicon + Intel) | First-class — primary dogfood platform | `/opt/homebrew/bin/ai-memory` (Apple Silicon Homebrew) or `/usr/local/bin/ai-memory` (Intel Homebrew) | `${HOME}/.claude/ai-memory.db` | `bash` (default) — Claude Code's `SessionStart` hook command runs in the user's default shell |
| **Linux** (glibc, x86_64 + aarch64) | First-class — covered by CI | `/usr/local/bin/ai-memory` (manual install) or `~/.cargo/bin/ai-memory` (cargo install) | `${HOME}/.claude/ai-memory.db` | `bash` |
| **Linux** (musl, e.g. Alpine) | Supported — static-linked binary recommended | per package manager | `${HOME}/.claude/ai-memory.db` | `sh`/`ash` — POSIX-compatible only |
| **Windows** (10/11, native) | Supported — see Windows-specific notes below | `C:\Users\<user>\.cargo\bin\ai-memory.exe` (cargo install) or wherever the user dropped the release zip | `%USERPROFILE%\.claude\ai-memory.db` | PowerShell or `cmd.exe`. `bash` only via WSL |
| **Windows** (WSL2) | First-class — equivalent to Linux | as Linux (above) | as Linux | `bash` |
| **Docker** / containers | First-class — official image planned, see "Container deployments" below | `/usr/local/bin/ai-memory` inside the image | `/data/ai-memory.db` (volume-mounted) | depends on host |
| **Kubernetes** | First-class — production deployment target, see "Kubernetes" below | `/usr/local/bin/ai-memory` inside the pod image | `/data/ai-memory.db` from a `PersistentVolumeClaim` (or `emptyDir` for ephemeral) | sidecar (HTTP boot) or DaemonSet (localhost:9077) |
| **ARM Linux** (Raspberry Pi, AWS Graviton, ARM servers) | First-class — covered by cross-compile docs, see "ARM Linux" below | per package manager / cargo install (`~/.cargo/bin/ai-memory`) | `${HOME}/.claude/ai-memory.db` | `bash`/`sh` |
| **Commercial Unix** (AIX, Solaris, HP-UX) | Best-effort — no project CI, "issues welcome but won't gate releases", see "Commercial Unix" below | varies (`/usr/local/bin/ai-memory` typical) | `${HOME}/.claude/ai-memory.db` | `sh`/`ksh` (POSIX) |
| **Embedded Linux** (OpenWRT, Yocto, Buildroot) | Best-effort — static-linked musl build, see "Embedded Linux" below | `/usr/bin/ai-memory` (per-package convention) | `/etc/ai-memory.db` or `/var/lib/ai-memory.db` (flash storage) | `sh`/`ash` (BusyBox POSIX) |
| **BSD** (FreeBSD, OpenBSD, NetBSD) | Best-effort — should build cleanly via `cargo build --release` but not regularly tested | `/usr/local/bin/ai-memory` (manual install) | `${HOME}/.claude/ai-memory.db` | `sh` |
| **iOS / Android** | Not supported | n/a | n/a | n/a |

> CI gap callout: the GitHub Actions matrix covers `ubuntu-latest`,
> `macos-latest`, and `windows-latest` only. Every other row above —
> Kubernetes, ARM Linux, commercial Unix, embedded Linux, BSD — is
> documented coverage, not CI-proven coverage. "First-class" for these
> means recipe-tested by maintainers and supported in the issue tracker;
> it does not mean every release is gated on a green build for that
> target. See the ["Lifetime test matrix" section](#lifetime-test-matrix-pr-3) below for what
> the CI actually exercises.

## macOS specifics

Most recipes in this directory assume macOS conventions (Homebrew binary,
`~/.claude/` config root). Production-tested on FROSTYi.local (Apple Silicon)
through the v0.6.3.1 dogfood workflow. No special notes — the recipes
"just work."

## Linux specifics

- The `ai-memory` binary is self-contained (statically links SQLite,
  bundles tokenizer assets in the binary). One-step install via
  `cargo install ai-memory` or via the release tarball.
- `~/.claude/` is the convention regardless of the agent host (same
  directory works for Claude Code on Linux, Cursor, Cline, etc.).
- For systemd-managed agents (running ai-memory as a daemon under a
  service unit), see [`docs/INSTALL.md`](../INSTALL.md). For session-boot
  integration the daemon mode is irrelevant — boot calls are stdio
  one-shots.

## Windows specifics

The integration recipes change on native Windows because
`SessionStart` hook commands run in PowerShell (or `cmd.exe`),
not in `bash`. Three things differ:

### 1. Path syntax in `~/.claude/settings.json`

Use forward slashes or escape backslashes — JSON requires escapes. Either
of these works:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "C:/Users/<user>/.cargo/bin/ai-memory.exe boot --quiet --limit 10"
          }
        ]
      }
    ]
  }
}
```

Or use the binary name alone if it's on `%PATH%`:

```json
{
  "command": "ai-memory boot --quiet --limit 10"
}
```

### 2. Default DB path env var

```json
{
  "env": {
    "AI_MEMORY_DB": "%USERPROFILE%\\.claude\\ai-memory.db"
  }
}
```

(Claude Code expands `%USERPROFILE%` before passing to the hook.)

### 3. PowerShell wrapper for the programmatic recipes

The `bash` snippets in
[`codex-cli.md`](codex-cli.md), [`claude-agent-sdk.md`](claude-agent-sdk.md),
etc. need PowerShell equivalents. Pattern:

```powershell
$bootContext = & ai-memory boot --quiet --limit 10 --format text 2>$null
if ($LASTEXITCODE -eq 0 -and $bootContext) {
    $systemMessage = "You are a helpful assistant.`n`n## Recent context (ai-memory)`n$bootContext"
} else {
    $systemMessage = "You are a helpful assistant."
}
```

(Same pattern works on Windows + Linux + macOS PowerShell 7+.)

## WSL2 specifics

Treat as Linux. The catch: each WSL distro has its own `~/.claude/` root.
If you also use Claude Code on the Windows side, you'll have two separate
ai-memory DBs unless you point both at the same path (e.g. via
`AI_MEMORY_DB=//wsl$/Ubuntu/home/<user>/.claude/ai-memory.db` from
Windows). Recommended: pick one side as the source of truth.

## Container deployments

Running ai-memory inside a container changes the DB persistence model:
without a volume mount, the DB lives inside the container and dies with
it. For session-boot integration the recipe pattern is:

```dockerfile
FROM rust:1.85-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --bin ai-memory

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/ai-memory /usr/local/bin/
VOLUME ["/data"]
ENV AI_MEMORY_DB=/data/ai-memory.db
ENTRYPOINT ["ai-memory"]
```

Then the host mounts `/data` to a persistent volume and the agent host
calls `docker exec <container> ai-memory boot --quiet` for the hook —
or, more commonly, runs `ai-memory` natively on the host and only uses
the container for the daemon mode.

The official image lives in `docker/Dockerfile` (TODO — track in #487
follow-ups).

## BSD specifics

`ai-memory` is expected to build and run on FreeBSD, OpenBSD, and NetBSD
via `cargo build --release` — Rust + rusqlite cover the platform — but is
not regularly tested. Treat as Linux for recipe purposes; file an issue
if you hit BSD-specific friction (path conventions, signal handling, FTS5
build flags) and we'll add explicit coverage.

## Kubernetes

Running `ai-memory` inside a Kubernetes cluster is a first-class
production deployment target. The session-boot model (`ai-memory boot`
returning recall context for an agent's first turn) maps to two
patterns: **sidecar** (per-pod) and **DaemonSet** (per-node). Both are
documented below, plus a Helm chart skeleton, ConfigMap-mounted config,
NetworkPolicy, and Secrets-based passphrase delivery.

### Sidecar pattern (per-pod ai-memory)

The agent and `ai-memory` run as containers in the same pod, sharing a
volume for the SQLite DB. The agent calls `ai-memory boot` either by
shelling into the sidecar (`kubectl exec`-style — only safe in dev) or
via the sidecar's local HTTP endpoint on `127.0.0.1:9077` (the
recommended production model).

When to use sidecar:
- Per-agent isolation. Each agent pod has its own DB lifecycle,
  passphrase, and namespace defaults.
- Short-lived workloads where DB sprawl across many pods is acceptable.
- Use `emptyDir` for ephemeral DB (recall context lives only as long as
  the pod), or a `PersistentVolumeClaim` for durable per-agent memory.

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: agent-with-ai-memory
spec:
  containers:
    - name: agent
      image: example/my-agent:latest
      env:
        - name: AI_MEMORY_HTTP
          value: "http://127.0.0.1:9077"
    - name: ai-memory
      image: ghcr.io/alphaonedev/ai-memory:0.6.3
      args: ["daemon", "--http", "0.0.0.0:9077"]
      env:
        - name: AI_MEMORY_DB
          value: "/data/ai-memory.db"
        - name: AI_MEMORY_CONFIG
          value: "/etc/ai-memory/config.toml"
      volumeMounts:
        - name: ai-memory-data
          mountPath: /data
        - name: ai-memory-config
          mountPath: /etc/ai-memory
          readOnly: true
        - name: ai-memory-passphrase
          mountPath: /run/secrets
          readOnly: true
  volumes:
    - name: ai-memory-data
      persistentVolumeClaim:
        claimName: ai-memory-pvc
    - name: ai-memory-config
      configMap:
        name: ai-memory-config
    - name: ai-memory-passphrase
      secret:
        secretName: ai-memory-passphrase
```

For ephemeral DB swap the PVC for `emptyDir: {}`.

### DaemonSet pattern (per-node ai-memory)

A single `ai-memory` instance runs on every node and listens on a
node-local socket (or `hostPort: 9077`). All agents on that node hit
`localhost:9077` for boot calls. Lower DB sprawl, single-node
consistency, simpler backup story.

When to use DaemonSet:
- Many small agents on the same node share recall context.
- You want one DB per node (per-namespace inside the DB segregates
  projects).
- You're already operating other DaemonSet observability/security
  agents and want symmetry.

```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: ai-memory
  namespace: ai-memory
spec:
  selector:
    matchLabels:
      app: ai-memory
  template:
    metadata:
      labels:
        app: ai-memory
    spec:
      hostNetwork: false
      containers:
        - name: ai-memory
          image: ghcr.io/alphaonedev/ai-memory:0.6.3
          args: ["daemon", "--http", "0.0.0.0:9077"]
          ports:
            - containerPort: 9077
              hostPort: 9077
          env:
            - name: AI_MEMORY_DB
              value: "/data/ai-memory.db"
            - name: AI_MEMORY_CONFIG
              value: "/etc/ai-memory/config.toml"
          volumeMounts:
            - name: ai-memory-data
              mountPath: /data
            - name: ai-memory-config
              mountPath: /etc/ai-memory
              readOnly: true
      volumes:
        - name: ai-memory-data
          hostPath:
            path: /var/lib/ai-memory
            type: DirectoryOrCreate
        - name: ai-memory-config
          configMap:
            name: ai-memory-config
```

### Helm chart skeleton

A minimal Helm chart structure for shipping `ai-memory` to a cluster.
The actual chart is **not** maintained in this repo today — see the
follow-up issue note below — but this skeleton is what you'd start from
if you're rolling your own chart.

```yaml
# Chart.yaml
apiVersion: v2
name: ai-memory
description: Persistent memory sidecar/daemon for AI agents
type: application
version: 0.1.0
appVersion: "0.6.3"
```

```yaml
# templates/deployment.yaml (DaemonSet variant)
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: {{ include "ai-memory.fullname" . }}
  labels:
    {{- include "ai-memory.labels" . | nindent 4 }}
spec:
  selector:
    matchLabels:
      {{- include "ai-memory.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "ai-memory.selectorLabels" . | nindent 8 }}
    spec:
      containers:
        - name: ai-memory
          image: "{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}"
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          args: ["daemon", "--http", "0.0.0.0:{{ .Values.service.port }}"]
          ports:
            - name: http
              containerPort: {{ .Values.service.port }}
              hostPort: {{ .Values.service.port }}
          env:
            - name: AI_MEMORY_DB
              value: {{ .Values.dbPath | quote }}
            - name: AI_MEMORY_CONFIG
              value: "/etc/ai-memory/config.toml"
          volumeMounts:
            - name: data
              mountPath: /data
            - name: config
              mountPath: /etc/ai-memory
              readOnly: true
      volumes:
        - name: data
          hostPath:
            path: {{ .Values.hostPath }}
            type: DirectoryOrCreate
        - name: config
          configMap:
            name: {{ include "ai-memory.fullname" . }}-config
```

> Helm chart shipping is **out of scope** for issue #487 PR-8. A proper
> chart with values schema, `helm lint` gating, and OCI-registry push is
> tracked as a follow-up issue (see the #487 thread for the cross-link).
> The skeleton above is illustrative; treat it as a starting point, not
> a supported artifact.

### ConfigMap-mounted config

Mount your `config.toml` from a ConfigMap so the binary picks it up via
`AI_MEMORY_CONFIG`:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: ai-memory-config
data:
  config.toml: |
    feature_tier = "keyword"
    archive_on_gc = true
    [recall]
    default_limit = 10
    default_budget_tokens = 4096
```

Pair with the volume mount shown in the sidecar / DaemonSet snippets:

```yaml
env:
  - name: AI_MEMORY_CONFIG
    value: "/etc/ai-memory/config.toml"
  - name: AI_MEMORY_DB
    value: "/data/ai-memory.db"
volumeMounts:
  - name: ai-memory-config
    mountPath: /etc/ai-memory
    readOnly: true
  - name: ai-memory-data
    mountPath: /data
```

### Boot hook in Kubernetes

Agents inside the cluster don't have direct access to `ai-memory boot`
as a stdio one-shot when `ai-memory` is running in a sidecar — there's
no shared filesystem unless you explicitly volume-share it, and no
shared shell. Two equivalents:

1. **HTTP boot (recommended for production).** `ai-memory daemon`
   exposes a boot endpoint. The agent fetches it at session start:

   ```bash
   curl -s "http://ai-memory:9077/v1/boot?namespace=my-project&limit=10&format=text"
   ```

   The response body is identical to `ai-memory boot --format text` —
   same status header, same body. Wire it into your agent the same way
   the [Codex CLI recipe](codex-cli.md) wires the local CLI.

2. **`kubectl exec` (dev only).** For interactive debugging, you can
   shell into the sidecar:

   ```bash
   kubectl exec -it agent-with-ai-memory -c ai-memory -- ai-memory boot --quiet --limit 10
   ```

   This is fine for poking at a running pod but **not** suitable for
   production: it requires `exec` RBAC on every pod, doesn't compose
   with stdio agents that fork once at startup, and will not work in
   read-only / locked-down clusters.

For stdio-only agents (no HTTP client), the current best practice is
the sidecar pattern with a shared `emptyDir` volume holding a Unix
socket, and `ai-memory daemon --unix-socket /run/ai-memory.sock` — but
that's outside the scope of issue #487 PR-8 and tracked as a separate
follow-up.

### NetworkPolicy

By default the daemon listens on the pod network only (no
`hostNetwork`). To restrict cross-namespace traffic so only agents in
the same namespace can hit `ai-memory`, attach a NetworkPolicy:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: ai-memory-restrict
  namespace: ai-memory
spec:
  podSelector:
    matchLabels:
      app: ai-memory
  policyTypes:
    - Ingress
  ingress:
    - from:
        - podSelector:
            matchLabels:
              role: agent
        - namespaceSelector:
            matchLabels:
              name: ai-memory
      ports:
        - protocol: TCP
          port: 9077
```

This locks ingress to pods labeled `role: agent` in the same namespace.
Adjust the selector for your topology.

### Secrets — SQLCipher passphrase

When the DB is SQLCipher-encrypted, deliver the passphrase as a
Kubernetes Secret mounted into the pod, never as a plain env var
(env vars leak into `kubectl describe` and into pod logs on crash).

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: ai-memory-passphrase
type: Opaque
stringData:
  passphrase: "replace-me-with-a-real-secret"
```

Mount it at `/run/secrets/ai-memory-passphrase` and point the binary
at it via the existing `--db-passphrase-file` flag (or
`AI_MEMORY_DB_PASSPHRASE_FILE` env var):

```yaml
volumeMounts:
  - name: ai-memory-passphrase
    mountPath: /run/secrets
    readOnly: true
env:
  - name: AI_MEMORY_DB_PASSPHRASE_FILE
    value: "/run/secrets/ai-memory-passphrase/passphrase"
```

The file-based flag avoids the passphrase appearing in process listings
or `env` output. See [`docs/INSTALL.md`](../INSTALL.md) for SQLCipher
setup details.

## ARM Linux (Raspberry Pi, AWS Graviton, others)

`ai-memory` builds and runs natively on 64-bit ARM Linux (aarch64) and
should also build on 32-bit ARM (armv7) for older Raspberry Pi
hardware. Apple Silicon (`aarch64-apple-darwin`) is already first-class
via the macOS dogfood path — this section covers Linux ARM
specifically.

### Native build (on the ARM device itself)

```bash
# 64-bit ARM (Pi 4/5, Graviton, ARM64 servers)
cargo build --release --target aarch64-unknown-linux-gnu

# 32-bit ARM (Pi 2/3 / Zero 2 W with armhf userland)
cargo build --release --target armv7-unknown-linux-gnueabihf
```

Native builds need a GCC toolchain (`apt install build-essential`) and
~2GB of free RAM during the link step. On a 1GB Pi you'll want to
cross-compile from a beefier host instead (see below).

### Cross-compile from x86_64

Add the target and a cross linker:

```bash
# Add Rust target
rustup target add aarch64-unknown-linux-gnu

# Linker (Linux x86_64 host)
sudo apt install gcc-aarch64-linux-gnu

# Linker (macOS x86_64 host — install ARM64 ELF cross GCC via Homebrew)
brew tap messense/macos-cross-toolchains
brew install aarch64-unknown-linux-gnu

# Build
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu
```

For armv7 (older Pis):

```bash
rustup target add armv7-unknown-linux-gnueabihf
sudo apt install gcc-arm-linux-gnueabihf
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
  cargo build --release --target armv7-unknown-linux-gnueabihf
```

### Tested known-good targets

These are the targets maintainers or contributors have actually
exercised at least once. **None are in the project's CI matrix** (which
covers `ubuntu-latest` x86_64, `macos-latest` arm64, `windows-latest`
x86_64) — treat the list as "known to compile and run a basic boot,"
not as continuously gated.

- `aarch64-unknown-linux-gnu` — Pi 4/5 with 64-bit Raspberry Pi OS, AWS
  Graviton2/3 instances, Ampere Altra servers.
- `aarch64-apple-darwin` — already first-class via macOS dogfood.
- `armv7-unknown-linux-gnueabihf` — Pi 3 with 32-bit Raspberry Pi OS.

If you build for a target not in this list and it works, please file an
issue so we can add it.

### DB path conventions

Same as Linux: `${HOME}/.claude/ai-memory.db` for per-user, or
`/var/lib/ai-memory/ai-memory.db` for system-wide. ARM doesn't change
filesystem layout.

### Resource notes

- **HNSW index is O(N) memory in embedding count.** On a Pi 4 / Pi 5
  with 4GB RAM, the semantic tier (which loads MiniLM and builds an
  HNSW index in process memory) will compete with everything else on
  the box. **Recommendation: start in `keyword` tier (FTS5 only, no
  embedder)** and only enable semantic / smart / autonomous tiers if
  you have headroom.
- **Build-time RAM.** Linking the release binary needs ~2GB; on a
  1GB Pi cross-compile from a host instead.
- **Storage.** SQLite WAL mode is fine on SD cards but writes more
  often than you'd expect — consider periodic checkpoints or an SSD if
  the Pi is heavily loaded.

## Commercial Unix (AIX, Solaris, HP-UX) — best-effort

`ai-memory` is **not** in the project's CI matrix on any commercial
Unix. This section documents what we know about the build path so users
on these platforms have a starting point — but issues filed against
these targets won't gate releases. The honest summary: try it, and if
it works file a positive report; if it doesn't, file an issue and
we'll help where we can.

### Build status

- **AIX (`powerpc64-unknown-aix`).** Rust nightly has had partial AIX
  target support since 2023; tier-3 last we checked. `cargo build`
  with a current nightly toolchain may succeed, may fail at SQLite
  link time depending on FTS5 flags. We have no first-hand build
  reports — issues welcome.
- **Solaris (`sparcv9-sun-solaris`, `x86_64-pc-solaris`).** Tier-2/3
  in Rust depending on toolchain. SQLite builds; rusqlite does too.
  Has been reported to work on Illumos derivatives; has not been
  exercised against vendor Solaris recently.
- **HP-UX (Itanium / PA-RISC).** No Rust target available upstream.
  Effectively unsupported until upstream Rust adds a target — we
  cannot ship a binary without one.

### Known issues

- **SQLite FTS5 build flags on AIX.** The default `rusqlite`
  `bundled` feature compiles SQLite from source; AIX's `xlc` and
  `gcc` flag handling can clash with the FTS5 amalgamation. Fallback:
  use the system SQLite via `--no-default-features --features sqlite`
  and link against a known-good libsqlite3.
- **`chflags` / append-only file mode (PR-5 audit log).** The audit
  log uses `chflags(2)` on macOS / BSD and the `chattr +a` ioctl on
  Linux to make the file append-only. **Solaris does not have either
  syscall surface** (different ACL / NFSv4 ACL system). On Solaris
  the audit log falls back to a no-op — the file is still written,
  but its append-only bit isn't enforced. AIX has its own JFS2
  immutable bit but we don't currently set it.
- **Signal handling.** SIGTERM / SIGINT behave normally; `SIGUSR1`
  (used for log rotation in PR-5) may behave differently on AIX
  under WPARs — untested.

### Recommended deployment

For commercial Unix shops, the **recommended path is containerized
x86_64 builds run inside an LPAR (AIX) or zone (Solaris)** with Linux
guests, rather than native compile. That moves you back onto the
first-class Linux build path and avoids the toolchain rabbit hole.
Native compile is reasonable only if the LPAR / zone option isn't
available for policy reasons.

### Path conventions

Same as Linux: `${HOME}/.claude/ai-memory.db` for per-user. On AIX you
may prefer `/var/ai-memory/` since `/var/lib/` isn't conventional;
override via `AI_MEMORY_DB`.

## Embedded Linux (OpenWRT, Yocto, Buildroot)

Running `ai-memory` on a router-class or embedded device is supported
on a best-effort basis via the static-linked musl build. The agents
running on these devices are typically tiny (LLM-as-router, IoT
gateway, on-device summarization), so the recall workload is modest —
the keyword tier is usually plenty.

### Build

Static-linked musl build, cross-compiled from a Linux x86_64 host:

```bash
rustup target add armv7-unknown-linux-musleabihf
# install musl cross toolchain (e.g. via musl.cc or buildroot SDK)
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=armv7l-linux-musleabihf-gcc \
  cargo build --release --target armv7-unknown-linux-musleabihf
```

Other useful targets:

- `aarch64-unknown-linux-musl` — modern 64-bit ARM routers (recent
  OpenWRT on aarch64 hardware).
- `mipsel-unknown-linux-musl`, `mips-unknown-linux-musl` — older
  MIPS-based OpenWRT routers. Rust target support is tier-3; expect
  rough edges.

The resulting binary is fully static and portable across musl
distributions of the same arch.

### Storage and audit log

- **Flash storage wear.** Embedded devices typically run from NAND or
  eMMC flash with limited write cycles. The audit log (PR-5) is the
  most write-heavy component. **Recommendation: pass `--max-size-mb 50`
  on the audit-log flag** to cap rotation size and avoid premature
  wear-leveling exhaustion. On very small devices (≤16 MB user
  storage) consider disabling the audit log entirely.
- **DB path.** `/var/lib/ai-memory.db` for systems with a writable
  `/var/lib`, or `/etc/ai-memory.db` on OpenWRT where `/etc` is the
  conventional persistent overlay.

### Memory budget

- **≤256 MB RAM devices: keyword tier only.** Don't enable the
  semantic or smart tiers — MiniLM weights alone are ~90 MB and the
  HNSW index grows linearly with memory count.
- **256 MB – 1 GB RAM: keyword tier recommended,** semantic possible
  if memory count stays small (<1k entries).
- **1 GB+ embedded boards (Pi 4 class):** treat as ARM Linux above.

## Lifetime test matrix (PR-3)

The session-boot lifetime test suite (PR-3 of issue #487) runs the
universal contract tests on a CI matrix:

- `ubuntu-latest` (Linux x86_64)
- `macos-latest` (Apple Silicon)
- `windows-latest` (native Windows)

Tests exercise: boot exit codes, status-header shape, recipe JSON
validity, namespace inference, budget clamp, status diagnostics. The live
agent smoke test (gated under `--features e2e`) currently runs only on
macOS where the dogfood Claude Code install lives; expanding to Linux + Windows
is tracked in #487 follow-ups.

**What CI does NOT cover** (be honest about the gap):

- Kubernetes pod lifecycle / Helm chart install — the YAML in this
  doc is illustrative, not gated. Production deployers should run
  their own `kubectl apply` smoke test.
- ARM Linux (`aarch64-unknown-linux-gnu`, `armv7-unknown-linux-gnueabihf`)
  — known-good per the section above, but not built or tested in CI.
- Commercial Unix (AIX, Solaris, HP-UX) — explicit best-effort, no CI.
- Embedded Linux (OpenWRT / musl cross-builds, MIPS targets) — no CI.
- BSD (FreeBSD / OpenBSD / NetBSD) — no CI.

If you operate on one of these targets and want to contribute a CI
runner (self-hosted GitHub Actions runner, etc.), please open an issue
referencing #487.

## Related

- [`README.md`](README.md) — agent matrix and the universal `ai-memory boot` primitive.
- [`../INSTALL.md`](../INSTALL.md) — full install instructions per platform.
- Issue #487 — RCA + lifetime suite + cross-files.
- Cross-section navigation: [Kubernetes](#kubernetes) ·
  [ARM Linux](#arm-linux-raspberry-pi-aws-graviton-others) ·
  [Commercial Unix](#commercial-unix-aix-solaris-hp-ux--best-effort) ·
  [Embedded Linux](#embedded-linux-openwrt-yocto-buildroot)
