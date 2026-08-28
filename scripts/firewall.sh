#!/usr/bin/env bash
# RhydaDB single-node firewall - block 9042 from the Internet, allow only via HAProxy (9043) and health (8080) locally.
# Tested on Ubuntu 24.04 (iptables + ufw). Run as root.
set -euo pipefail

# Allow loopback
iptables -A INPUT -i lo -j ACCEPT

# Allow established
iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT

# Allow SSH (adjust port if needed)
iptables -A INPUT -p tcp --dport 22 -j ACCEPT

# Allow HAProxy TLS front (public)
iptables -A INPUT -p tcp --dport 9043 -j ACCEPT

# Allow health/metrics only from localhost and monitoring subnet (adjust 10.0.0.0/8)
iptables -A INPUT -p tcp -s 127.0.0.1 --dport 8080 -j ACCEPT
iptables -A INPUT -p tcp -s 10.0.0.0/8 --dport 8080 -j ACCEPT
iptables -A INPUT -p tcp --dport 8080 -j DROP

# Block direct CQL (9042) from the Internet - only HAProxy on same host should use it
iptables -A INPUT -p tcp --dport 9042 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 9042 -j DROP

# Drop everything else (or set default policy)
# iptables -P INPUT DROP
# iptables -P FORWARD DROP
# iptables -P OUTPUT ACCEPT

echo "Firewall applied: 9042 blocked from Internet, 9043 public (TLS), 8080 localhost/10.0.0.0/8 only"

# UFW alternative (simpler, if you use UFW instead of raw iptables):
# ufw default deny incoming
# ufw allow 22/tcp
# ufw allow 9043/tcp
# ufw allow from 127.0.0.1 to any port 8080
# ufw allow from 10.0.0.0/8 to any port 8080
# ufw deny 8080
# ufw deny 9042
# ufw --force enable
# ufw status numbered
