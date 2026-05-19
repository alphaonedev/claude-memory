#cloud-config
# Track E1 — IronClaw agent droplet bootstrap. Templated by main.tf.
# Each agent registers itself with the shared ai-memory substrate at
# ${memory_private_ip}:9077 using its per-droplet `agent_id`.
package_update: true
packages:
  - curl
  - jq
write_files:
  - path: /etc/systemd/system/ironclaw-agent.service
    permissions: '0644'
    content: |
      [Unit]
      Description=IronClaw v0.28.1 agent #${agent_index} (Track E1 hive)
      After=network-online.target
      Wants=network-online.target

      [Service]
      Type=simple
      User=ironclaw
      Group=ironclaw
      Environment=AI_MEMORY_AGENT_ID=hive-e1-agent-${agent_index}
      Environment=AI_MEMORY_HTTP=http://${memory_private_ip}:9077
      Environment=XAI_API_KEY=__OPERATOR_INJECTED_AT_BOOT__
      ExecStart=/opt/ironclaw/bin/ironclaw --provider openai-compatible --base-url https://api.x.ai/v1 --model grok-4.3
      Restart=on-failure
      RestartSec=5

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
  # Service is NOT enabled by default — operator runs `ironclaw-agent start`
  # via the post-spawn playbook so XAI_API_KEY can be injected at start time
  # rather than embedded in the unit file (the cloud-init pass would leak it
  # to disk in the systemd unit).
