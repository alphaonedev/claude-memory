// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// =============================================================================
// Track E1 — Digital Ocean CPU agent hive (issue #833).
// =============================================================================
//
// Status: MONEY-GATED. This Terraform manifest is IaC-only — no `terraform
// apply` is performed by AI agents. The operator triggers spend explicitly
// (see `infra/do-hive/spawn.sh` for the wrapped invocation).
//
// Cost estimate (NYC3, on-demand droplets, 2026 pricing as published at
// https://www.digitalocean.com/pricing/droplets):
//
//   resource            qty  price/hr   total/hr   total/24h   total/month
//   -------------------------------------------------------------------------
//   ai-memory droplet   1    $0.024     $0.024     $0.58       $17.41
//     (s-1vcpu-2gb, postgres + AGE + ai-memory daemon)
//   agent droplet       N    $0.012     $0.012N    $0.29N      $8.70N
//     (s-1vcpu-1gb, IronClaw runner)
//   vpc + firewall      —    $0         $0         $0          $0
//   inference (xAI Grok 4.3 API)            offload — billed per-token to operator's xAI account
//
// Worked totals (N = number of agent droplets):
//
//   N = 4     → $0.072/hr  → $1.73/24h  → ~$51/month
//   N = 10    → $0.144/hr  → $3.46/24h  → ~$104/month  ← reference "hive"
//   N = 25    → $0.324/hr  → $7.78/24h  → ~$234/month
//   N = 50    → $0.624/hr  → $14.98/24h → ~$449/month
//
// Variable `agent_count` defaults to 10 (the operator-defended reference
// hive size from #833's D1-D5 demo capture brief). Bump for larger
// emergent-behavior runs; the math above scales linearly.
//
// Smoke-test playbook (~$2 budget):
//   1. operator: `infra/do-hive/spawn.sh apply` (1h smoke, agent_count=4)
//   2. agents come online via cloud-init bootstrap
//   3. capture D1-D5 outputs (cross-agent memory, recursive reflection)
//   4. operator: `infra/do-hive/teardown.sh` (idempotent)
//
// Audit hook: every spawn writes the resolved droplet IDs + IPs +
// SHA256(droplet_user_data) to `.local-runs/do-hive-runs/<ts>/` so a
// post-mortem can reconstruct exactly which agent saw what code.
//
// Money-gate enforcement: `spawn.sh` REQUIRES the env var
// `AI_MEMORY_OPERATOR_DO_SPEND_APPROVED=1` to be set before delegating
// to `terraform apply`. AI NHI agents MUST NOT set this var; only the
// human operator does.
// =============================================================================

terraform {
  required_version = ">= 1.5.0"
  required_providers {
    digitalocean = {
      source  = "digitalocean/digitalocean"
      version = "~> 2.40"
    }
  }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------
//
// Token is sourced from the operator's `DIGITALOCEAN_TOKEN` env var. NEVER
// commit a token to this file. The `spawn.sh` wrapper re-exports the var
// from the operator's keychain (1Password / op vault) so the value never
// touches a repo file.

provider "digitalocean" {}

// ---------------------------------------------------------------------------
// Variables
// ---------------------------------------------------------------------------

variable "region" {
  description = "DO region. NYC3 has the lowest sustained-rate cost as of 2026-05."
  type        = string
  default     = "nyc3"
}

variable "agent_count" {
  description = "Number of agent droplets to spawn. Reference hive: 10 (≈$0.144/hr)."
  type        = number
  default     = 10
}

variable "memory_droplet_size" {
  description = "Slug for the shared ai-memory + postgres droplet."
  type        = string
  default     = "s-1vcpu-2gb"
}

variable "agent_droplet_size" {
  description = "Slug for each agent droplet (IronClaw runner)."
  type        = string
  default     = "s-1vcpu-1gb"
}

variable "ssh_pubkey_fingerprint" {
  description = "SSH key fingerprint to authorise on every droplet. Operator's key."
  type        = string
}

variable "ai_memory_image_url" {
  description = "URL to the pre-built ai-memory release tarball (operator-published)."
  type        = string
  default     = "https://github.com/alphaonedev/ai-memory-mcp/releases/latest/download/ai-memory-x86_64-unknown-linux-gnu.tar.gz"
}

variable "ironclaw_image_url" {
  description = "URL to the IronClaw v0.28.1 runner tarball."
  type        = string
  default     = "https://github.com/alphaonedev/ironclaw/releases/download/v0.28.1/ironclaw-x86_64-unknown-linux-gnu.tar.gz"
}

// ---------------------------------------------------------------------------
// VPC — isolates the hive's east-west traffic from public internet
// ---------------------------------------------------------------------------

resource "digitalocean_vpc" "hive" {
  name     = "ai-memory-hive-${var.region}"
  region   = var.region
  ip_range = "10.10.0.0/16"
}

// ---------------------------------------------------------------------------
// ai-memory droplet (shared substrate)
// ---------------------------------------------------------------------------
//
// Runs postgres + Apache AGE + the ai-memory autonomous-tier daemon on
// :9077. Bound to the VPC private IP so only agent droplets in the same
// VPC can reach it; no public ingress on :9077.

resource "digitalocean_droplet" "memory" {
  image    = "ubuntu-24-04-x64"
  name     = "ai-memory-hive-substrate"
  region   = var.region
  size     = var.memory_droplet_size
  vpc_uuid = digitalocean_vpc.hive.id
  ssh_keys = [var.ssh_pubkey_fingerprint]

  user_data = templatefile("${path.module}/cloud-init-memory.yaml.tpl", {
    ai_memory_image_url = var.ai_memory_image_url
  })

  tags = ["ai-memory-hive", "ai-memory-substrate"]
}

// ---------------------------------------------------------------------------
// Agent droplets — IronClaw runners
// ---------------------------------------------------------------------------

resource "digitalocean_droplet" "agent" {
  count    = var.agent_count
  image    = "ubuntu-24-04-x64"
  name     = "ai-memory-hive-agent-${count.index + 1}"
  region   = var.region
  size     = var.agent_droplet_size
  vpc_uuid = digitalocean_vpc.hive.id
  ssh_keys = [var.ssh_pubkey_fingerprint]

  user_data = templatefile("${path.module}/cloud-init-agent.yaml.tpl", {
    ironclaw_image_url = var.ironclaw_image_url
    memory_private_ip  = digitalocean_droplet.memory.ipv4_address_private
    agent_index        = count.index + 1
  })

  tags = ["ai-memory-hive", "ai-memory-agent"]
}

// ---------------------------------------------------------------------------
// Firewall — east-west only on :9077, ssh from operator IP only
// ---------------------------------------------------------------------------

resource "digitalocean_firewall" "hive" {
  name = "ai-memory-hive-fw"

  droplet_ids = concat(
    [digitalocean_droplet.memory.id],
    digitalocean_droplet.agent[*].id,
  )

  // SSH from operator only (operator sets DO_FIREWALL_SSH_SOURCES via env)
  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = [getenv("DO_FIREWALL_SSH_SOURCES")]
  }

  // East-west on :9077 (ai-memory HTTP daemon)
  inbound_rule {
    protocol             = "tcp"
    port_range           = "9077"
    source_droplet_ids   = digitalocean_droplet.agent[*].id
  }

  // Allow all outbound (agents call xAI Grok API)
  outbound_rule {
    protocol              = "tcp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }

  outbound_rule {
    protocol              = "udp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
}

// ---------------------------------------------------------------------------
// Outputs — re-used by spawn.sh + the post-run audit dump
// ---------------------------------------------------------------------------

output "memory_public_ip" {
  value = digitalocean_droplet.memory.ipv4_address
}

output "memory_private_ip" {
  value = digitalocean_droplet.memory.ipv4_address_private
}

output "agent_ips" {
  value = digitalocean_droplet.agent[*].ipv4_address
}

output "monthly_cost_estimate_usd" {
  value = format(
    "memory(%.2f) + agents(%.2fx%d) = %.2f/month",
    17.41,
    8.70,
    var.agent_count,
    17.41 + (8.70 * var.agent_count),
  )
}
