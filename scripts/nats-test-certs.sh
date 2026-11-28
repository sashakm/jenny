#!/usr/bin/env bash
# Certificates and a server configuration for the TLS NATS the backend tests use
# (ADR-031): `just test-nats` and CI run it, then start `nats:2-alpine` with
# `-c /certs/nats.conf` and `<dir>` mounted at `/certs`.
#
# Usage: scripts/nats-test-certs.sh <dir>
#
# Writes, into <dir>:
#   ca.crt, ca.key                  the test CA
#   server.crt, server.key          signed by it, for `localhost` and `127.0.0.1`
#   client-a.crt, client-a.key      signed by it, subject CN=jenny-test-client
#   client-b.crt, client-b.key      the same subject with another key: a rotation
#   other-ca.crt, other-ca.key      a CA the server does not trust
#   other.crt, other.key            the same subject, signed by the other CA
#   nats.conf                       mTLS with `verify_and_map`, one user, no anonymous access
#
# For tests only: every key is world-readable, so the container's user can read them.
set -euo pipefail

dir="${1:?usage: $0 <dir>}"
mkdir -p "$dir"
cd "$dir"

subject="/CN=jenny-test-client"
days=30

key() {
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1.key" 2>/dev/null
}

ca() {
  key "$1"
  openssl req -x509 -new -key "$1.key" -subj "/CN=$2" -days "$days" -out "$1.crt" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
}

# leaf <name> <ca> <subject> <extensions>
leaf() {
  key "$1"
  openssl req -new -key "$1.key" -subj "$3" -out "$1.csr" 2>/dev/null
  printf '%s\n' "$4" >"$1.ext"
  openssl x509 -req -in "$1.csr" -CA "$2.crt" -CAkey "$2.key" -CAcreateserial \
    -days "$days" -extfile "$1.ext" -out "$1.crt" 2>/dev/null
  rm -f "$1.csr" "$1.ext"
}

client_ext="basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth"

ca ca "jenny test CA"
ca other-ca "jenny untrusted test CA"
leaf server ca "/CN=localhost" "basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1"
leaf client-a ca "$subject" "$client_ext"
leaf client-b ca "$subject" "$client_ext"
leaf other other-ca "$subject" "$client_ext"
rm -f ./*.srl
chmod 644 ./*.key ./*.crt

cat >nats.conf <<'EOF'
port: 4222
jetstream {
  store_dir: /tmp/jetstream
}
tls {
  cert_file: /certs/server.crt
  key_file: /certs/server.key
  ca_file: /certs/ca.crt
  # The client's certificate is required, and its subject is the user.
  verify_and_map: true
  timeout: 2
}
authorization {
  users = [
    { user: "CN=jenny-test-client" }
  ]
}
EOF
chmod 644 nats.conf
