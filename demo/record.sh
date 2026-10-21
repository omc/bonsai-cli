#!/usr/bin/env bash
# Records docs/assets/demo.gif against bonsai-mock, dressed as production.
#
# The mock runs inside a private user, network, mount and PID namespace (no
# root, nothing on the host changes). In there api.bonsai.io resolves to
# 127.0.0.1, a local TLS proxy signed by a throwaway CA answers on :443, and
# /home is an empty tmpfs with a /home/demo home. The `bonsai` the tape types is
# a wrapper that runs the real binary inside that namespace, so the CLI talks to
# its default https://api.bonsai.io/v1 and nothing local shows on camera.
#
# Run from the repo root:   nix shell nixpkgs#vhs -c demo/record.sh
# Other output, e.g. a quick MP4: nix shell nixpkgs#vhs -c demo/record.sh -o /tmp/demo.mp4
# Dry run without VHS:      DRY_RUN='bash my-steps.sh' demo/record.sh
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d)
cleanup() {
  [ -n "${HOLDER:-}" ] && kill -KILL "$HOLDER" 2>/dev/null
  if [ -n "${KEEP_WORK:-}" ]; then echo "kept $WORK"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT

cargo build -q -p bonsai -p bonsai-mock
# Outside /home, which the namespace hides.
mkdir -p "$WORK/real"
cp target/debug/bonsai target/debug/bonsai-mock demo/fake_search.py demo/prepare.py "$WORK/real/"
REAL=$WORK/real

# --- a throwaway CA and a certificate for api.bonsai.io
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=bonsai demo CA" \
  -keyout "$WORK/ca.key" -out "$WORK/ca.pem" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=api.bonsai.io" \
  -keyout "$WORK/api.key" -out "$WORK/api.csr" 2>/dev/null
printf 'subjectAltName=DNS:api.bonsai.io\nextendedKeyUsage=serverAuth\n' >"$WORK/ext"
openssl x509 -req -in "$WORK/api.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" \
  -CAcreateserial -days 1 -extfile "$WORK/ext" -out "$WORK/api.pem" 2>/dev/null
# ... and one for the clusters' own hosts, served by demo/fake_search.py
openssl req -newkey rsa:2048 -nodes -subj "/CN=*.us-east-1.bonsaisearch.net" \
  -keyout "$WORK/search.key" -out "$WORK/search.csr" 2>/dev/null
printf 'subjectAltName=DNS:*.us-east-1.bonsaisearch.net\nextendedKeyUsage=serverAuth\n' >"$WORK/ext2"
openssl x509 -req -in "$WORK/search.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" \
  -CAcreateserial -days 1 -extfile "$WORK/ext2" -out "$WORK/search.pem" 2>/dev/null

# --- name resolution inside the namespace: api.bonsai.io is local, files only.
# A cluster's host is added once it exists (point_dns, below), at 127.0.0.2.
printf '127.0.0.1 localhost\n127.0.0.1 api.bonsai.io\n' >"$WORK/hosts"
sed 's/^hosts:.*/hosts: files/' /etc/nsswitch.conf >"$WORK/nsswitch.conf"

# --- the namespace: mock on :4010, TLS proxy on :443, an empty /home/demo.
# Its own PID namespace, so killing the holder takes everything in it down.
unshare -rn --mount --pid --fork --kill-child sh -c "
  ip link set lo up
  mount --bind '$WORK/hosts' /etc/hosts
  mount --bind '$WORK/nsswitch.conf' /etc/nsswitch.conf
  mount -t tmpfs tmpfs /home
  mkdir -p /home/demo
  '$REAL/bonsai-mock' --db '$WORK/mock.sqlite' serve --port 4010 --step-ms 400 --reset \
    --public-url https://api.bonsai.io --checkout-url https://checkout.stripe.com/c/pay/ \
    >'$WORK/mock.out' 2>&1 &
  socat openssl-listen:443,bind=127.0.0.1,reuseaddr,fork,cert='$WORK/api.pem',key='$WORK/api.key',verify=0 \
    tcp:127.0.0.1:4010 &
  python3 '$REAL/fake_search.py' '$WORK/search.pem' '$WORK/search.key' &
  wait
" </dev/null >"$WORK/ns.log" 2>&1 &
HOLDER=$!
for _ in $(seq 100); do grep -q 'admin token' "$WORK/mock.out" 2>/dev/null && break; sleep 0.1; done

# --- commands that run inside the namespace, from /home/demo
mkdir -p "$WORK/bin"
wrap() {
  cat >"$WORK/bin/$1" <<WRAP
#!/bin/sh
exec nsenter -t $HOLDER --user --net --mount --preserve-credentials -- \\
  /bin/sh -c 'cd /home/demo && exec "\$0" "\$@"' $2 "\$@"
WRAP
  chmod +x "$WORK/bin/$1"
}
wrap bonsai-ns "$REAL/bonsai"
wrap curl "$(command -v curl)"
wrap nspython "$(command -v python3)"
# The recording leaves out login's "No keychain here" warning (the demo stores
# tokens in a file on purpose). CLICOLOR_FORCE keeps colour through the pipe.
cat >"$WORK/bin/bonsai" <<'EOF'
#!/usr/bin/env bash
if [ "${1:-}" = login ]; then
  CLICOLOR_FORCE=1 bonsai-ns "$@" 2> >(grep -v 'No keychain here' >&2)
  rc=$?
  wait
  exit "$rc"
fi
exec bonsai-ns "$@"
EOF
chmod +x "$WORK/bin/bonsai"

# --- globex has no card; its token can write clusters
GLOBEX_TOKEN=$("$REAL/bonsai-mock" --db "$WORK/mock.sqlite" token create --account globex \
  --scopes "account:read clusters:read clusters:write credentials:read" --name demo)
ADMIN=$(grep '^admin token' "$WORK/mock.out" | awk '{print $NF}')

cat >"$WORK/demo.env" <<EOF
export PATH="$WORK/bin:\$PATH"
export HOME=/home/demo
unset XDG_CONFIG_HOME
export TOKEN=$GLOBEX_TOKEN
export BONSAI_CA_BUNDLE=$WORK/ca.pem
export CURL_CA_BUNDLE=$WORK/ca.pem
export BONSAI_KEYRING=off
export DEMO_DB=$WORK/mock.sqlite DEMO_TOKEN=$GLOBEX_TOKEN DEMO_ADMIN=$ADMIN
# off camera: add a card for globex, and have the next charge declined
add_card_declined() {
  curl -s -X PUT -H "Authorization: Bearer $ADMIN" \\
    -H 'Content-Type: application/json' -d true https://api.bonsai.io/_mock/accounts/globex/payment_method
  '$REAL/bonsai-mock' --db '$WORK/mock.sqlite' scenario set fail_next payment_failed >/dev/null
}
# off camera: resolve the cluster's host to the fake search server, and write
# products.ndjson sized to match what the mock will report
prepare_data() {
  host=\$(bonsai url "\$1" | sed -E 's#^https://[^@]*@##')
  echo "127.0.0.2 \$host" >>'$WORK/hosts'
  nspython '$REAL/prepare.py' dataset "\$1"
}
# off camera: the cluster's stats catch up with the data just loaded
sync_stats() { nspython '$REAL/prepare.py' stats "\$1"; }
EOF

if [ -n "${DRY_RUN:-}" ]; then
  DEMO_ENV="$WORK/demo.env" bash -c "$DRY_RUN"
else
  DEMO_ENV="$WORK/demo.env" vhs demo/demo.tape "$@"
fi
