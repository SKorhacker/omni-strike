#!/usr/bin/env bash
# One-time preparation of a fresh Ubuntu 22.04/24.04 or Debian 12 VPS for Omni Strike.
# Run AS ROOT on the VPS (you, not the app):   bash vps-bootstrap.sh <deploy-user>
#
# Does: OS updates + automatic security updates, Docker Engine + Compose plugin (official repo),
# firewall (SSH, 80, 443 only), fail2ban for SSH, a non-root deploy user in the docker group with
# YOUR existing root authorized_keys, and SSH password login disabled.
# Does NOT: create secrets, clone the repo, or start anything (see docs/deployment.md).
set -euo pipefail

USER_NAME="${1:?usage: bash vps-bootstrap.sh <deploy-user>}"
[[ $EUID -eq 0 ]] || { echo "run as root"; exit 1; }
[[ -s /root/.ssh/authorized_keys ]] || { echo "no /root/.ssh/authorized_keys: add your SSH public key first (password login gets disabled)"; exit 1; }

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get -y upgrade
apt-get install -y ca-certificates curl gnupg ufw fail2ban unattended-upgrades git
dpkg-reconfigure -f noninteractive unattended-upgrades

# Docker Engine from Docker's repository (distro packages lag behind; compose >= 2.24.4 needed).
. /etc/os-release
install -m 0755 -d /etc/apt/keyrings
curl -fsSL "https://download.docker.com/linux/${ID}/gpg" -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/${ID} ${VERSION_CODENAME} stable" \
  > /etc/apt/sources.list.d/docker.list
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker

# Container logs are already capped per service in the compose file; cap the daemon default too.
cat > /etc/docker/daemon.json <<'JSON'
{ "log-driver": "json-file", "log-opts": { "max-size": "20m", "max-file": "5" }, "live-restore": true }
JSON
systemctl restart docker

# Firewall. NOTE: ports published by Docker bypass ufw; the compose files publish only 80/443
# publicly (Caddy) and everything else on 127.0.0.1.
ufw default deny incoming
ufw default allow outgoing
ufw allow OpenSSH
ufw allow 80/tcp
ufw allow 443/tcp
ufw allow 443/udp
ufw --force enable
systemctl enable --now fail2ban

# Deploy user (no password, your SSH key, docker group).
if ! id "$USER_NAME" >/dev/null 2>&1; then
  adduser --disabled-password --gecos "" "$USER_NAME"
fi
usermod -aG docker "$USER_NAME"
install -d -m 700 -o "$USER_NAME" -g "$USER_NAME" "/home/$USER_NAME/.ssh"
install -m 600 -o "$USER_NAME" -g "$USER_NAME" /root/.ssh/authorized_keys "/home/$USER_NAME/.ssh/authorized_keys"

# SSH: keys only.
cat > /etc/ssh/sshd_config.d/10-omni-strike.conf <<'CONF'
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
CONF
systemctl reload ssh 2>/dev/null || systemctl reload sshd

docker --version
docker compose version
echo
echo "OK. Next, as $USER_NAME:  git clone <repo> && cd <repo>/deploy  (then docs/deployment.md, 'Mainnet non-custodial on a VPS')"
