#!/bin/bash

set -e

cd "$(dirname "${0}")" || exit 1

# OpenSSL 4 no longer falls back to the CN for hostname checks, so the
# certificate must carry a subjectAltName.
openssl req -nodes -new -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -sha256 -days 3650 -extensions v3_req -addext "subjectAltName=DNS:www.example.com" -subj "/C=US/ST=CA/L=SF/O=Company/OU=Org/CN=www.example.com"
openssl pkcs12 -export -inkey key.pem -in cert.pem -out identity.pfx -passout pass:ntex
cp key.pem cert.pem identity.pfx ../../ntex-tls/examples/
