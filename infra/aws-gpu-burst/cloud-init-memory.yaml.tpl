#cloud-config
# Track E2 — ai-memory + postgres + AGE bootstrap on the burst substrate.
package_update: true
packages:
  - postgresql-16
  - postgresql-server-dev-16
  - build-essential
  - git
  - curl
  - jq
write_files:
  - path: /etc/systemd/system/ai-memory.service
    permissions: '0644'
    content: |
      [Unit]
      Description=ai-memory MCP daemon (Track E2 burst substrate)
      After=postgresql.service network-online.target
      Wants=postgresql.service network-online.target

      [Service]
      Type=simple
      User=aimemory
      Group=aimemory
      Environment=AI_MEMORY_PERMISSIONS_MODE=enforce
      Environment=AI_MEMORY_AUTONOMOUS_HOOKS=1
      ExecStart=/opt/ai-memory/bin/ai-memory serve --bind 0.0.0.0:9077 --store-url postgres://aimemory:CHANGEME@localhost/aimemory
      Restart=on-failure

      [Install]
      WantedBy=multi-user.target
runcmd:
  - useradd -m -d /opt/ai-memory -s /bin/bash aimemory
  - mkdir -p /opt/ai-memory/bin
  - chown -R aimemory:aimemory /opt/ai-memory
  - curl -fsSL "${ai_memory_image_url}" -o /tmp/ai-memory.tar.gz
  - tar -xzf /tmp/ai-memory.tar.gz -C /opt/ai-memory/bin
  - chmod 0755 /opt/ai-memory/bin/ai-memory
  - sudo -u postgres psql -c "CREATE USER aimemory WITH PASSWORD 'CHANGEME';"
  - sudo -u postgres psql -c "CREATE DATABASE aimemory OWNER aimemory;"
  - sudo -u postgres psql -d aimemory -c "CREATE EXTENSION IF NOT EXISTS age;"
  - systemctl daemon-reload
  - systemctl enable --now ai-memory
