#!/bin/sh
# Run the non-desktop test suite on x86_64 Linux without network access: cross-build the test
# binaries with cargo-zigbuild (glibc 2.28), then run them in a Debian container (Docker) with the
# repository mounted at the same path, so compile-time fixture paths resolve.
set -eu

sideport_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
sideport_target=x86_64-unknown-linux-gnu.2.28
sideport_work=$(mktemp -d)
trap 'rm -rf "$sideport_work"' EXIT
cd "$sideport_root"

cat > "$sideport_work/Dockerfile" <<'DOCKERFILE'
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 unzip procps \
    && rm -rf /var/lib/apt/lists/*
DOCKERFILE
docker build --quiet --platform linux/amd64 -t sideport-linux-test "$sideport_work" > /dev/null

cargo-zigbuild test --target "$sideport_target" --no-run --workspace --exclude sl-app --exclude sl-macos \
    --message-format=json 2> "$sideport_work/build.log" \
    | python3 -c 'import json, sys
for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if message.get("reason") == "compiler-artifact" and message.get("executable") and message["profile"]["test"]:
        print(message["executable"])' > "$sideport_work/binaries"

cat > "$sideport_work/run.sh" <<'RUN'
#!/bin/sh
export HOME=/tmp/home
mkdir -p "$HOME"
cd "$1"
status=0
while read -r binary; do
    "$binary" --test-threads=4 || status=1
done < "$2"
exit $status
RUN

docker run --rm --network none --platform linux/amd64 \
    -v "$sideport_root:$sideport_root" -v "$sideport_work:$sideport_work" \
    sideport-linux-test sh "$sideport_work/run.sh" "$sideport_root" "$sideport_work/binaries"
