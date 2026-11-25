#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

arch="${1:-amd64}"
case "$arch" in
  amd64|arm64) ;;
  *)
    echo "Usage: $0 [amd64|arm64]" >&2
    exit 1
    ;;
esac

out="dist/linux-${arch}"
bin="${out}/redmine-dev-mcp"

docker build \
  --platform "linux/${arch}" \
  --target bin \
  --output "type=local,dest=${out}" \
  .

file "$bin"

set +e
output="$(docker run --rm --platform "linux/${arch}" \
  -v "$PWD/${bin}:/redmine-dev-mcp:ro" \
  ubuntu:22.04 /redmine-dev-mcp 2>&1)"
status=$?
set -e

if [[ "$status" -ne 1 ]] || [[ "$output" != *"REDMINE_URL is required"* ]]; then
  echo "Smoke test failed (exit ${status}):" >&2
  echo "$output" >&2
  exit 1
fi

echo "Verified ${bin} on ubuntu:22.04"
