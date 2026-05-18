#cloud-config
# Track E2 — IronClaw agent bootstrap. Each agent talks MCP stdio with
# IronClaw, which calls the vLLM endpoint over openai-compatible HTTP
# (so the agent code is unchanged from the Track E1 DO variant; only
# the inference base URL is swapped from xAI to local vLLM).
package_update: true
packages:
  - curl
  - jq
write_files:
  - path: /etc/systemd/system/ironclaw-agent.service
    permissions: '0644'
    content: |
      [Unit]
      Description=IronClaw v0.28.1 agent #${agent_index} (Track E2 burst)
      After=network-online.target
      Wants=network-online.target

      [Service]
      Type=simple
      User=ironclaw
      Group=ironclaw
      Environment=AI_MEMORY_AGENT_ID=burst-e2-agent-${agent_index}
      Environment=AI_MEMORY_HTTP=http://${memory_private_ip}:9077
      ExecStart=/opt/ironclaw/bin/ironclaw --provider openai-compatible --base-url http://${vllm_private_ip}:8000/v1 --model auto
      Restart=on-failure

      [Install]
      WantedBy=multi-user.target
runcmd:
  - useradd -m -d /opt/ironclaw -s /bin/bash ironclaw
  - mkdir -p /opt/ironclaw/bin
  - chown -R ironclaw:ironclaw /opt/ironclaw
  - curl -fsSL "${ironclaw_image_url}" -o /tmp/ironclaw.tar.gz
  - tar -xzf /tmp/ironclaw.tar.gz -C /opt/ironclaw/bin
  - chmod 0755 /opt/ironclaw/bin/ironclaw
  - systemctl daemon-reload
  # NOT enabled by default — operator starts via post-spawn playbook
  # after confirming vLLM is healthy (vLLM cold-start takes ~3-5min
  # while it downloads/warms the model).
