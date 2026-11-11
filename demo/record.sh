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
cp target/debug/bonsai target/debug/bonsai-mock demo/prepare.py "$WORK/real/"
REAL=$WORK/real

# --- a throwaway CA and a certificate for api.bonsai.io
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=bonsai demo CA" \
  -keyout "$WORK/ca.key" -out "$WORK/ca.pem" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=api.bonsai.io" \
  -keyout "$WORK/api.key" -out "$WORK/api.csr" 2>/dev/null
printf 'subjectAltName=DNS:api.bonsai.io\nextendedKeyUsage=serverAuth\n' >"$WORK/ext"
openssl x509 -req -in "$WORK/api.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" \
  -CAcreateserial -days 1 -extfile "$WORK/ext" -out "$WORK/api.pem" 2>/dev/null

# --- name resolution inside the namespace: api.bonsai.io is local, files only.
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
    --public-url https://api.bonsai.io \
    >'$WORK/mock.out' 2>&1 &
  socat openssl-listen:443,bind=127.0.0.1,reuseaddr,fork,cert='$WORK/api.pem',key='$WORK/api.key',verify=0 \
    tcp:127.0.0.1:4010 &
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
wrap bonsai "$REAL/bonsai"
wrap nspython "$(command -v python3)"

# --- acme has every scope and a card on file
TOKEN=$("$REAL/bonsai-mock" --db "$WORK/mock.sqlite" token create --account acme \
  --scopes "account:read clusters:read clusters:write credentials:read" --name demo)
ADMIN=$(grep '^admin token' "$WORK/mock.out" | awk '{print $NF}')

cat >"$WORK/demo.env" <<EOF
export PATH="$WORK/bin:\$PATH"
export HOME=/home/demo
unset XDG_CONFIG_HOME
export BONSAI_CA_BUNDLE=$WORK/ca.pem
export BONSAI_KEYRING=off
export DEMO_DB=$WORK/mock.sqlite DEMO_TOKEN=$TOKEN DEMO_ADMIN=$ADMIN
# off camera: log in with acme's token, kept in a file as there is no keychain here
demo_login() { echo $TOKEN | bonsai login --with-token --store-file >/dev/null 2>&1; }
# off camera: give the cluster a week of history and some data to report
add_history() { nspython '$REAL/prepare.py' "\$1"; }
EOF

if [ -n "${DRY_RUN:-}" ]; then
  DEMO_ENV="$WORK/demo.env" bash -c "$DRY_RUN"
else
  DEMO_ENV="$WORK/demo.env" vhs demo/demo.tape "$@"
fi
