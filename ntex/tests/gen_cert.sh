#!/bin/bash

set -e

cd "$(dirname "${0}")" || exit 1

openssl req -nodes -new -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -sha256 -days 3650 -extensions v3_req -subj "/C=US/ST=CA/L=SF/O=Company/OU=Org/CN=www.example.com"
