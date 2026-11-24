#!/usr/bin/env bash
# Exercise MQTT and mutual TLS against a disposable EMQX broker on loopback.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $# -gt 1 || ( $# -eq 1 && $1 != --with-smgw ) ]]; then
    echo "usage: scripts/check-mqtt.sh [--with-smgw]" >&2
    exit 2
fi
broker_image='emqx/emqx@sha256:a1e3d10fa1dcc8c94325b815a93b5b5974253a97cb36470b44e47d5d3ef9cd13'
test_dir=$(mktemp -d)
container=
plain=
cleanup() {
    if [[ -n "$container" ]]; then docker rm --force "$container" >/dev/null; fi
    if [[ -n "$plain" ]]; then docker rm --force "$plain" >/dev/null; fi
    rm -rf "$test_dir"
}
trap cleanup EXIT
umask 077
for authority in ca untrusted; do
    openssl req -new -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
        -nodes -days 1 -subj "/CN=Ephemeral test $authority" \
        -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' \
        -keyout "$test_dir/$authority.key" -out "$test_dir/$authority.crt"
done
for endpoint in server device-a device-b backend; do
    usage=clientAuth
    if [[ "$endpoint" == server ]]; then usage=serverAuth; fi
    openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -keyout "$test_dir/$endpoint.key" -out "$test_dir/$endpoint.csr" -subj "/CN=$endpoint"
    cat > "$test_dir/extensions" <<EXT
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=$usage
subjectAltName=IP:127.0.0.1
EXT
    openssl x509 -req -days 1 -in "$test_dir/$endpoint.csr" -CA "$test_dir/ca.crt" \
        -CAkey "$test_dir/ca.key" -CAcreateserial -extfile "$test_dir/extensions" -out "$test_dir/$endpoint.crt"
done
# Expose only the ephemeral broker identity and public CA to its unprivileged UID.
mkdir "$test_dir/listener"
cp "$test_dir/ca.crt" "$test_dir/server.crt" "$test_dir/server.key" "$test_dir/listener/"
chmod 755 "$test_dir/listener"
chmod 444 "$test_dir/listener/"*
# Anonymous protocol scenarios use a different broker. The identity-bound
# broker exposes a single MQTT listener and accepts only verified certificates.
plain=$(docker run -d --publish 127.0.0.1::1883 "$broker_image")
docker cp "$plain:/opt/emqx/etc/emqx.conf" "$test_dir/emqx.conf"
cat interop/mqtt-acl/emqx.conf >> "$test_dir/emqx.conf"
chmod 444 "$test_dir/emqx.conf"
cp interop/mqtt-acl/acl.conf "$test_dir/listener/"
chmod 444 "$test_dir/listener/acl.conf"
container=$(docker run -d --publish 127.0.0.1::8883 \
    --mount "type=bind,src=$test_dir/listener,dst=/test-listener,readonly" \
    --mount "type=bind,src=$test_dir/emqx.conf,dst=/opt/emqx/etc/emqx.conf,readonly" \
    -e "EMQX_NODE__COOKIE=$(openssl rand -hex 32)" "$broker_image")
for ((n=0;n<60;n++)); do
    if docker exec "$container" emqx ctl listeners 2>/dev/null | grep '^ssl:default' >/dev/null &&
        docker exec "$plain" emqx ctl listeners 2>/dev/null | grep '^tcp:default' >/dev/null; then break; fi
    if [[ $(docker inspect -f '{{.State.Running}}' "$container") != true ]] || ((n == 59)); then
        docker logs --tail 20 "$container" >&2
        exit 1
    fi
    sleep 1
done
export CLS_EEDI_TEST_PKI="$test_dir"
port=$(docker port "$plain" 1883/tcp)
export CLS_EEDI_BROKER_PORT="${port##*:}"
port=$(docker port "$container" 8883/tcp)
export CLS_EEDI_TLS_PORT="${port##*:}"
cargo test --locked --features mqtt --test transport independent_mqtt5 -- --ignored --test-threads=1
if [[ ${1:-} == --with-smgw ]]; then
    cargo test --locked --manifest-path interop/smgw/Cargo.toml -- --include-ignored
fi
