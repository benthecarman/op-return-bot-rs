#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: walletnotify.sh TXID" >&2
  exit 64
fi

: "${ORB_ADMIN_KEY_FILE:?set ORB_ADMIN_KEY_FILE to the shared key file}"
ORB_WALLETNOTIFY_URL="${ORB_WALLETNOTIFY_URL:-http://127.0.0.1:9000/admin/walletnotify}"
ORB_WALLETNOTIFY_KEY=$(tr -d '\r\n' < "$ORB_ADMIN_KEY_FILE")

exec curl --fail --silent --show-error \
  --header "Content-Type: application/json" \
  --data "{\"txid\":\"$1\",\"key\":\"$ORB_WALLETNOTIFY_KEY\"}" \
  "$ORB_WALLETNOTIFY_URL"
