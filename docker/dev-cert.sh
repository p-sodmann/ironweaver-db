#!/bin/sh
# A self-signed CA and a server certificate for development (step 15b,
# ADR 0048), for compose.yaml: it mounts docker/tls at /etc/iwdb/tls.
#
#   sh docker/dev-cert.sh                    # docker/tls/{ca,server}.{pem,key}
#   sh docker/dev-cert.sh --client admin     # also client-admin.{pem,key} (mTLS as admin)
#   sh docker/dev-cert.sh --name db.example.internal   # also that name in the certificate
#
# NOT for production: the CA's key lies next to it, the keys are readable by
# every user (the container's user, uid 10001, must read the server's key
# from the mounted directory), and nothing renews them. docker/tls is in
# .gitignore and .dockerignore; no certificate goes into the image.
#
# The server's certificate names localhost, 127.0.0.1, ::1 and iwdb (the
# compose service). Trust docker/tls/ca.pem in your client (iwctl --tls-ca,
# iwdb.connect(ca=...), curl --cacert) or browser to use it. Needs OpenSSL
# 1.1.1 or later, or LibreSSL 3 (macOS; -sha256 everywhere: its default
# digest is SHA-1, which clients refuse). Running it again keeps the CA and
# makes new certificates; delete docker/tls to start over.
set -eu
out="$(cd "$(dirname "$0")" && pwd)/tls"
days=825
names="DNS:localhost,DNS:iwdb,IP:127.0.0.1,IP:::1"
clients=""
while [ $# -gt 0 ]; do
    case "$1" in
        --client) clients="$clients $2"; shift 2 ;;
        --name) names="$names,DNS:$2"; shift 2 ;;
        -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
        *) echo "dev-cert.sh: unknown argument '$1' (see --help)" >&2; exit 2 ;;
    esac
done
mkdir -p "$out"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

key() { openssl ecparam -name prime256v1 -genkey -noout 2>/dev/null | openssl pkcs8 -topk8 -nocrypt -out "$1"; }

if [ ! -f "$out/ca.pem" ] || [ ! -f "$out/ca.key" ]; then
    key "$out/ca.key"
    printf 'basicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n' >"$work/ca.ext"
    openssl req -new -sha256 -key "$out/ca.key" -subj "/O=Ironweaver DB development/CN=iwdb dev CA" -out "$work/ca.csr"
    openssl x509 -req -sha256 -in "$work/ca.csr" -signkey "$out/ca.key" -extfile "$work/ca.ext" -days 3650 \
        -out "$out/ca.pem" 2>/dev/null
    echo "made a development CA: $out/ca.pem"
fi

# name (file), subject, extensions
cert() {
    key "$out/$1.key"
    printf '%s' "$3" >"$work/$1.ext"
    openssl req -new -sha256 -key "$out/$1.key" -subj "$2" -out "$work/$1.csr"
    openssl x509 -req -sha256 -in "$work/$1.csr" -CA "$out/ca.pem" -CAkey "$out/ca.key" -set_serial "0x$(openssl rand -hex 8)" \
        -extfile "$work/$1.ext" -days "$days" -out "$out/$1.pem" 2>/dev/null
    chmod 644 "$out/$1.pem" "$out/$1.key"
}

cert server "/O=Ironweaver DB development/CN=localhost" \
    "$(printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=%s\n' "$names")"
echo "made the server's certificate: $out/server.pem ($names), key $out/server.key"
for user in $clients; do
    cert "client-$user" "/O=Ironweaver DB development/CN=$user" \
        "$(printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n')"
    echo "made a client certificate for the user $user: $out/client-$user.pem, key $out/client-$user.key"
done
chmod 600 "$out/ca.key"
