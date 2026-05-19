#cloud-config
# Track E2 — vLLM inference node bootstrap (g5.2xlarge, A10G 24GB).
# Templated by main.tf. Money-gated; operator triggers.
package_update: true
packages:
  - python3-pip
  - curl
  - git
  - nvidia-driver-550
write_files:
  - path: /etc/systemd/system/vllm.service
    permissions: '0644'
    content: |
      [Unit]
      Description=vLLM inference server (Track E2)
      After=network-online.target
      Wants=network-online.target

      [Service]
      Type=simple
      User=vllm
      Group=vllm
      Environment=HF_HOME=/opt/hf-cache
      ExecStart=/opt/vllm/venv/bin/python -m vllm.entrypoints.openai.api_server \
        --model ${vllm_model} \
        --host 0.0.0.0 \
        --port 8000 \
        --max-model-len 4096
      Restart=on-failure
      RestartSec=10

      [Install]
      WantedBy=multi-user.target
runcmd:
  - useradd -m -d /opt/vllm -s /bin/bash vllm
  - mkdir -p /opt/vllm /opt/hf-cache
  - chown -R vllm:vllm /opt/vllm /opt/hf-cache
  - sudo -u vllm python3 -m venv /opt/vllm/venv
  - sudo -u vllm /opt/vllm/venv/bin/pip install --upgrade pip
  - sudo -u vllm /opt/vllm/venv/bin/pip install vllm==0.5.5
  - reboot  # required for nvidia-driver to load before vLLM starts
