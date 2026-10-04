#!/bin/sh
# Test-only certificates and keys for the TLS and mTLS tests (step 15b).
# NOT for any deployment: the keys are public, in this repository.
#
#   sh tests/fixtures/tls/make.sh     # needs OpenSSL 3.4 or later (-not_before)
#
# Valid certificates last until 2125, so the fixtures don't expire; the
# expired ones expired in 2001. The CA keys aren't kept: run the script
# again to make a new set.
set -eu
cd "$(dirname "$0")"
umask 022
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

key() { openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1" 2>/dev/null; }

ca() { # name, subject
    key "$work/$1.key"
    printf 'basicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n' >"$work/$1.ext"
    openssl req -new -key "$work/$1.key" -subj "$2" -out "$work/$1.csr"
    openssl x509 -req -in "$work/$1.csr" -signkey "$work/$1.key" -extfile "$work/$1.ext" \
        -not_before 20260101000000Z -not_after 21250101000000Z -set_serial 0x$(openssl rand -hex 8) -out "$1.pem" 2>/dev/null
}

# name, subject, ca, server|client, not_before, not_after
cert() {
    key "$1.key"
    if [ "$4" = server ]; then
        printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1\n' >"$work/$1.ext"
    else
        printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n' >"$work/$1.ext"
    fi
    openssl req -new -key "$1.key" -subj "$2" -out "$work/$1.csr"
    openssl x509 -req -in "$work/$1.csr" -CA "$3.pem" -CAkey "$work/$3.key" -extfile "$work/$1.ext" \
        -not_before "$5" -not_after "$6" -set_serial 0x$(openssl rand -hex 8) -out "$1.pem" 2>/dev/null
}

valid="20260101000000Z 21250101000000Z"
expired="20000101000000Z 20010101000000Z"

ca ca "/O=Ironweaver DB tests/CN=Test CA"
ca other-ca "/O=Somebody else/CN=Other CA"
cert server "/O=Ironweaver DB tests/CN=localhost" ca server $valid
cert server-renewed "/O=Ironweaver DB tests/OU=renewed/CN=localhost" ca server $valid
cert server-expired "/O=Ironweaver DB tests/CN=localhost" ca server $expired
cert server-other-ca "/O=Somebody else/CN=localhost" other-ca server $valid
cert client-admin "/O=Ironweaver DB tests/CN=admin" ca client $valid
cert client-ann "/O=Ironweaver DB tests/OU=people/CN=ann" ca client $valid
cert client-bob "/O=Ironweaver DB tests/CN=bob" ca client $valid
cert client-nobody "/O=Ironweaver DB tests/CN=nobody" ca client $valid
cert client-no-cn "/O=Ironweaver DB tests/OU=no common name" ca client $valid
cert client-two-cn "/O=Ironweaver DB tests/CN=ann/CN=admin" ca client $valid
cert client-expired "/O=Ironweaver DB tests/CN=ann" ca client $expired
cert client-other-ca "/O=Somebody else/CN=admin" other-ca client $valid
echo "made the test certificates in $(pwd)"
