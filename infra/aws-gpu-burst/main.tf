// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// =============================================================================
// Track E2 — AWS GPU burst hive (issue #834).
// =============================================================================
//
// Status: MONEY-GATED. IaC-only. Operator triggers spend; AI NHI agents do not.
//
// Cost estimate (us-east-1, spot pricing as published at
// https://aws.amazon.com/ec2/spot/pricing/, May 2026):
//
//   resource                pricing       qty   total/hr
//   ------------------------------------------------------
//   g5.2xlarge spot (vLLM)  ~$0.60/hr     1     $0.60
//     (A10G 24GB VRAM, Llama-3.1-8B / Qwen2.5-7B)
//   t3.medium (agent)       $0.0416/hr    5     $0.21
//   t3.large (ai-memory+pg) $0.0832/hr    1     $0.08
//   EBS gp3 (~100GB)        $0.008/hr     7     $0.06
//   data transfer           negligible    —     ~$0.01
//   ------------------------------------------------------
//   TOTAL                                       ~$0.96/hr
//
// Scenarios:
//
//   - 24h smoke              → $23
//   - 48h D1-D5 demo capture → $46
//   - 72h full campaign      → $69
//   - 10-agent variant 72h   → $84
//
// $200 cap from #834 mandate → $130 reserve for re-runs / spot interruption
// recovery / scaling to 10-agent emergent-behavior runs.
//
// Money-gate enforcement: `spawn.sh` REQUIRES env var
// `AI_MEMORY_OPERATOR_AWS_SPEND_APPROVED=1`. AI NHI agents MUST NOT set
// this var; only the human operator does.
// =============================================================================

terraform {
  required_version = ">= 1.5.0"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.40"
    }
  }
}

provider "aws" {
  region = var.region
}

// ---------------------------------------------------------------------------
// Variables
// ---------------------------------------------------------------------------

variable "region" {
  description = "AWS region. us-east-1 typically has the lowest g5.2xlarge spot price."
  type        = string
  default     = "us-east-1"
}

variable "agent_count" {
  description = "Number of t3.medium agent instances. Demo brief targets 5; emergent-behavior variant goes to 10."
  type        = number
  default     = 5
}

variable "vllm_model" {
  description = "HuggingFace model id for the vLLM inference node. Llama-3.1-8B is the bench reference."
  type        = string
  default     = "meta-llama/Llama-3.1-8B-Instruct"
}

variable "ssh_key_name" {
  description = "EC2 key pair name authorised on every instance. Operator's key."
  type        = string
}

variable "ssh_source_cidr" {
  description = "CIDR allowlist for SSH ingress. Operator's IP."
  type        = string
}

variable "ai_memory_image_url" {
  description = "URL to the ai-memory release tarball."
  type        = string
  default     = "https://github.com/alphaonedev/ai-memory-mcp/releases/latest/download/ai-memory-x86_64-unknown-linux-gnu.tar.gz"
}

variable "ironclaw_image_url" {
  description = "URL to the IronClaw v0.28.1 runner tarball."
  type        = string
  default     = "https://github.com/alphaonedev/ironclaw/releases/download/v0.28.1/ironclaw-x86_64-unknown-linux-gnu.tar.gz"
}

// ---------------------------------------------------------------------------
// Networking — dedicated VPC for the burst test so teardown is clean
// ---------------------------------------------------------------------------

resource "aws_vpc" "burst" {
  cidr_block           = "10.20.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true
  tags = { Name = "ai-memory-burst-hive", Project = "ai-memory-track-e2" }
}

resource "aws_internet_gateway" "burst" {
  vpc_id = aws_vpc.burst.id
  tags   = { Project = "ai-memory-track-e2" }
}

resource "aws_subnet" "burst" {
  vpc_id                  = aws_vpc.burst.id
  cidr_block              = "10.20.1.0/24"
  map_public_ip_on_launch = true
  availability_zone       = "${var.region}a"
  tags                    = { Project = "ai-memory-track-e2" }
}

resource "aws_route_table" "burst" {
  vpc_id = aws_vpc.burst.id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.burst.id
  }
  tags = { Project = "ai-memory-track-e2" }
}

resource "aws_route_table_association" "burst" {
  subnet_id      = aws_subnet.burst.id
  route_table_id = aws_route_table.burst.id
}

resource "aws_security_group" "burst" {
  name        = "ai-memory-burst-hive-sg"
  description = "Track E2 burst hive: ssh from operator, east-west on 8000/9077"
  vpc_id      = aws_vpc.burst.id

  ingress {
    description = "SSH from operator IP only"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [var.ssh_source_cidr]
  }

  ingress {
    description = "vLLM HTTP (within VPC only)"
    from_port   = 8000
    to_port     = 8000
    protocol    = "tcp"
    cidr_blocks = [aws_vpc.burst.cidr_block]
  }

  ingress {
    description = "ai-memory HTTP (within VPC only)"
    from_port   = 9077
    to_port     = 9077
    protocol    = "tcp"
    cidr_blocks = [aws_vpc.burst.cidr_block]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

// ---------------------------------------------------------------------------
// AMI lookups — Ubuntu 24.04 LTS (Canonical-published)
// ---------------------------------------------------------------------------

data "aws_ami" "ubuntu_2404" {
  most_recent = true
  owners      = ["099720109477"] // Canonical

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"]
  }
}

// ---------------------------------------------------------------------------
// vLLM inference node — g5.2xlarge spot
// ---------------------------------------------------------------------------

resource "aws_spot_instance_request" "vllm" {
  ami                  = data.aws_ami.ubuntu_2404.id
  instance_type        = "g5.2xlarge"
  key_name             = var.ssh_key_name
  subnet_id            = aws_subnet.burst.id
  vpc_security_group_ids = [aws_security_group.burst.id]
  wait_for_fulfillment = true
  spot_type            = "one-time"

  // ~$0.60/hr; bump this if region quote is hotter than the estimate
  spot_price = "0.75"

  user_data = templatefile("${path.module}/cloud-init-vllm.yaml.tpl", {
    vllm_model = var.vllm_model
  })

  root_block_device {
    volume_type = "gp3"
    volume_size = 100
  }

  tags = { Name = "ai-memory-burst-vllm", Project = "ai-memory-track-e2" }
}

// ---------------------------------------------------------------------------
// ai-memory + postgres node — t3.large (on-demand; spot is unreliable for stateful)
// ---------------------------------------------------------------------------

resource "aws_instance" "memory" {
  ami                    = data.aws_ami.ubuntu_2404.id
  instance_type          = "t3.large"
  key_name               = var.ssh_key_name
  subnet_id              = aws_subnet.burst.id
  vpc_security_group_ids = [aws_security_group.burst.id]

  user_data = templatefile("${path.module}/cloud-init-memory.yaml.tpl", {
    ai_memory_image_url = var.ai_memory_image_url
  })

  root_block_device {
    volume_type = "gp3"
    volume_size = 100
  }

  tags = { Name = "ai-memory-burst-substrate", Project = "ai-memory-track-e2" }
}

// ---------------------------------------------------------------------------
// Agent fleet — t3.medium spot
// ---------------------------------------------------------------------------

resource "aws_spot_instance_request" "agent" {
  count                = var.agent_count
  ami                  = data.aws_ami.ubuntu_2404.id
  instance_type        = "t3.medium"
  key_name             = var.ssh_key_name
  subnet_id            = aws_subnet.burst.id
  vpc_security_group_ids = [aws_security_group.burst.id]
  wait_for_fulfillment = true
  spot_type            = "one-time"
  spot_price           = "0.06"

  user_data = templatefile("${path.module}/cloud-init-agent.yaml.tpl", {
    ironclaw_image_url = var.ironclaw_image_url
    vllm_private_ip    = aws_spot_instance_request.vllm.private_ip
    memory_private_ip  = aws_instance.memory.private_ip
    agent_index        = count.index + 1
  })

  tags = { Name = "ai-memory-burst-agent-${count.index + 1}", Project = "ai-memory-track-e2" }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

output "vllm_public_ip" {
  value = aws_spot_instance_request.vllm.public_ip
}

output "memory_public_ip" {
  value = aws_instance.memory.public_ip
}

output "agent_public_ips" {
  value = aws_spot_instance_request.agent[*].public_ip
}

output "hourly_cost_estimate_usd" {
  value = format(
    "vLLM(0.60) + agents(0.0416x%d) + memory(0.08) + ebs+misc(0.08) = %.2f/hr",
    var.agent_count,
    0.60 + (0.0416 * var.agent_count) + 0.08 + 0.08,
  )
}
