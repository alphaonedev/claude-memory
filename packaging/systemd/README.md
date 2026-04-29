# ai-memory systemd units

Drop-in systemd units for operators running ai-memory as a hardened
single-node deployment. Shipped by the Debian PPA and Fedora COPR
packages; also usable standalone on any systemd distro.

## Units

| File | Purpose | Type |
|------|---------|------|
| `ai-memory.service` | Main daemon (HTTP + MCP) | `simple` |
| `ai-memory-sync.service` | Peer-mesh sync daemon (optional) | `simple` |
| `ai-memory-backup.service` | One-shot snapshot via `VACUUM INTO` | `oneshot` |
| `ai-memory-backup.timer` | Hourly backup trigger | `timer` |

## Install — manual

```sh
# 1. System user + state dir. The Debian PPA postinst and Fedora COPR
#    %post scriptlet do this automatically.
sudo useradd --system --home /var/lib/ai-memory --shell /usr/sbin/nologin ai-memory
sudo install -d -o ai-memory -g ai-memory -m 0750 /var/lib/ai-memory
sudo install -d -o ai-memory -g ai-memory -m 0750 /var/lib/ai-memory/backups

# 2. Units into /etc/systemd/system (or /usr/lib/systemd/system for distro packages)
sudo install -m 0644 packaging/systemd/*.service /etc/systemd/system/
sudo install -m 0644 packaging/systemd/*.timer   /etc/systemd/system/

# 3. Reload + enable.
sudo systemctl daemon-reload
sudo systemctl enable --now ai-memory.service
sudo systemctl enable --now ai-memory-backup.timer
```

## Sync daemon — optional

The `ai-memory-sync.service` is disabled by default. Configure peers via
`/etc/ai-memory/sync.env`:

```sh
PEERS=https://peer-a.example:9077,https://peer-b.example:9077
# For mTLS, add --client-cert / --client-key / --mtls-allowlist:
EXTRA_ARGS=--client-cert /etc/ai-memory/tls/client.pem --client-key /etc/ai-memory/tls/client.key
```

Then:

```sh
sudo systemctl enable --now ai-memory-sync.service
```

## Hardening

All units ship with maximally restrictive systemd sandboxing:

- No new privileges
- Strict filesystem — read-only system, only `/var/lib/ai-memory` writable
- No access to `/home`, `/tmp` (private), `/dev` (private)
- No kernel tunables, modules, logs, cgroups
- Address families restricted to `AF_UNIX AF_INET AF_INET6`
- `SystemCallFilter=@system-service` with `@mount @swap @reboot @obsolete` denied
- Capability bounding set empty
- Memory Deny Write Execute (no JIT)

Review `systemd-analyze security ai-memory.service` to verify exposure level.
Ship-default target: "OK" or better (score <5.0).

## Resource caps

Default caps are tuned for a single-node operator running a modest
load. Override via a drop-in at
`/etc/systemd/system/ai-memory.service.d/override.conf`:

```ini
[Service]
MemoryMax=8G
TasksMax=2048
LimitNOFILE=131072
```

Do not weaken hardening directives without understanding the tradeoff —
if an exploit lands in a crate deep in the dep tree, these are the walls
that keep it from pivoting.

## Troubleshooting

```sh
# Runtime status
systemctl status ai-memory
journalctl -u ai-memory -n 200 -f

# Sandboxing review
systemd-analyze security ai-memory.service
systemd-analyze verify /etc/systemd/system/ai-memory.service

# Backup verification
ls -la /var/lib/ai-memory/backups
sudo -u ai-memory /usr/bin/ai-memory backup list --to /var/lib/ai-memory/backups
```

## License

Apache-2.0. See `../../LICENSE`.
