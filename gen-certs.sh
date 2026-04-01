#!/bin/sh
# Generate self-signed TLS certificates for RDoWS development.
# Outputs server.crt and server.key in the current directory.
#
# Usage:
#   ./gen-certs.sh                  # localhost only
#   ./gen-certs.sh 10.1.0.22        # include a routable IP for two-host use

set -e

IP="${1:-}"

if [ -n "$IP" ]; then
    SAN="DNS:localhost,IP:127.0.0.1,IP:$IP"
    CN="$IP"
else
    SAN="DNS:localhost,IP:127.0.0.1"
    CN="localhost"
fi

openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout server.key \
    -out server.crt \
    -days 365 \
    -subj "/CN=$CN" \
    -addext "subjectAltName=$SAN" \
    -addext "basicConstraints=CA:FALSE"

echo "Generated server.crt and server.key (CN=$CN, SAN=$SAN)"
echo ""
echo "Start the server:"
echo "  cargo run -p rdows-server -- --bind 0.0.0.0:9443 --cert server.crt --key server.key"
