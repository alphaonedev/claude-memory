# Networking — running `ai-memory` on macOS + VPNs

This page collects the third-party-networking gotchas that operators
have hit when running `ai-memory` (daemon or CLI) on macOS hosts that
also run a VPN client. It is **operator-awareness documentation**: the
substrate has no programmable workaround for any of these — they are
all routing-layer behaviours imposed by the VPN client on processes
that aren't Apple-notarized.

## macOS Tailscale per-app intercept

**Filed as.** [`ai-memory-mcp` #704](https://github.com/alphaonedev/ai-memory-mcp/issues/704),
discovered during the v0.7.0 Plan B / Plan D test-cell coordination
(`#700` SHIP CAMPAIGN).

### Symptom

Outbound TCP from Homebrew `psql`, the Rust `ai-memory` binary, and
other non-Apple-signed processes to a LAN IP (e.g. `192.168.50.1`)
fails with `EHOSTUNREACH`. The same address responds fine from
`nc(1)`, `ssh(1)`, and Safari, so the failure looks at first glance
like an `ai-memory` regression rather than a routing-table issue.

Typical operator-visible failure modes:

- `ai-memory serve --store-url postgres://user:pass@192.168.50.1/db`
  fails to start with a connect error before the daemon binds its
  HTTP listener.
- `psql -h 192.168.50.1 -U <user> <db>` from a Homebrew install fails
  with `could not connect to server: No route to host`.
- `cargo test --features sal,sal-postgres` that points at a LAN
  Postgres host times out, even with `pg_hba.conf` fully open.

### Diagnosis

`tailscale status` shows a tailnet-assigned IPv4 address (CGNAT range,
typically `100.x.y.z`) alongside the LAN address. macOS Tailscale
installs a system-level NetworkExtension that performs per-app
interception of LAN-range packets:

- Apple-signed binaries (Safari, Finder, `nc(1)`, `ssh(1)` from
  `/usr/bin`) bypass the extension and reach the LAN IP normally.
- Non-Apple-signed binaries (Homebrew tools, `cargo`-built Rust
  binaries, Docker for Mac, Python from `pyenv`) are routed through
  the extension, which returns `EHOSTUNREACH` for LAN destinations
  it hasn't been instructed to allow.

The behaviour is documented at the NEAR AI / Apple / Tailscale
notarization layer and is not actionable from inside `ai-memory`.

### Workaround — use the tailnet address

For any non-Apple-signed binary that needs to talk to a LAN host that
is also on your tailnet, use the **tailnet IP** instead of the LAN IP:

```bash
# Bad — LAN IP, fails from psql / ai-memory:
psql -h 192.168.50.1 -U fed_user fed_meta

# Good — tailnet IP, works from psql / ai-memory:
psql -h 100.70.167.11 -U fed_user fed_meta
```

On the Postgres side, allow the CGNAT tailnet range in `pg_hba.conf`
alongside (not instead of) the LAN range:

```text
# /etc/postgresql/16/main/pg_hba.conf
host  fed_meta  fed_user  192.168.50.0/24   scram-sha-256
host  fed_meta  fed_user  100.64.0.0/10     scram-sha-256
```

The CGNAT block (`100.64.0.0/10`) covers every Tailscale-assigned
address; you don't need to enumerate per-node IPs.

### Long-term outlook

No substrate-level fix. The NEAR AI / Apple / Tailscale notarization
landscape would need to change — either Tailscale ships its extension
with broader allowlisting for unsigned binaries, or Apple's
notarization policy changes — and neither is actionable from this
project. Operator can disable the gotcha by reconfiguring Tailscale
to leave the affected subnet un-intercepted, but the default install
on macOS reproduces the failure mode.

### Operator probe

If you suspect this is what you're hitting, the quickest check is:

```bash
# Apple-signed binary — should succeed:
nc -zv 192.168.50.1 5432   # or whatever LAN-IP:port

# Non-Apple-signed binary — should fail with EHOSTUNREACH:
/opt/homebrew/opt/postgresql@16/bin/psql -h 192.168.50.1 -U fed_user -c 'SELECT 1' fed_meta
```

If `nc` succeeds and `psql` fails with `No route to host`, you have a
Tailscale per-app intercept. Swap to the tailnet address.

### Cross-references

- **`ai-memory-a2a-v0.7.0`** branch
  [`plan-d-mac-mini-f2-native`](https://github.com/alphaonedev/ai-memory-a2a-v0.7.0/tree/plan-d-mac-mini-f2-native/plan-d)
  — the localcell setup that originally hit this; `plan-d/README.md`
  has the test-cell-specific topology and `setup-f2.sh` / `setup-mac-mini.sh`.
- **Issue #704** — the gap issue this page was opened against.
- **`docs/integrations/README.md`** — the broader integrations index.
