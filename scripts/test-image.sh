#!/usr/bin/env bash
# Run only on a disposable Docker test host. Requires Docker Engine 28+.
set -euo pipefail
image=${1:?Provide the image to test}
socket_group=$(stat -c '%g' /var/run/docker.sock)
run_fixture() {
  local script=$1; shift
  docker run --rm -i --user 10001:10001 --group-add "$socket_group" --cap-drop ALL \
    --security-opt no-new-privileges:true --volume /var/run/docker.sock:/var/run/docker.sock:ro \
    --read-only --tmpfs /tmp:rw,nosuid,size=512m --memory 2g --pids-limit 256 \
    "$@" --entrypoint node "$image" - < "$script"
}
for key in '' fixture-inference-key '!printf unexpected $HOME ${PATH}'; do
  run_fixture tests/uat/runner-protocol.cjs --env "POUNDLOCK_TEST_INFERENCE_KEY=$key"
done
run_fixture tests/uat/runner-protocol.cjs --env POUNDLOCK_TEST_MODEL_ERROR=1
run_fixture tests/uat/runner-protocol.cjs --env POUNDLOCK_TEST_WORKER_ISOLATION=1 --env POUNDLOCK_TEST_INFERENCE_KEY=fixture-isolation-key
docker run --rm --user 10001:10001 --cap-drop ALL --read-only \
  --tmpfs /tmp:rw,exec,nosuid,nodev,size=64m,uid=10001,gid=10001,mode=0700 \
  --tmpfs /work:rw,exec,nosuid,nodev,size=64m,uid=10001,gid=10001,mode=0700 \
  --network none --entrypoint bash "$image" -lc \
  'python3.14 -c "import sys; assert sys.version_info[:2]==(3,14)"; uv --version; printf "#!/bin/sh\nexit 0\n" > /tmp/ok && chmod +x /tmp/ok && /tmp/ok; printf "#!/bin/sh\nexit 0\n" > /work/ok && chmod +x /work/ok && /work/ok'
for mode in concurrency cancel restart deadline verify check-failed protected repair repair-limit missing command-help check-timeout verify-cancel verify-restart repair-restart network network-blocked combined python-uv reasoning-deltas git; do
  run_fixture tests/uat/worker-lifecycle.cjs --env "POUNDLOCK_TEST_WORKER_MODE=$mode"
done
docker run --rm -i --network none --user 10001:10001 --cap-drop ALL \
  --read-only --tmpfs /tmp:rw,nosuid,size=64m --memory 128m --pids-limit 16 \
  --entrypoint node "$image" - < tests/uat/wake-relay.cjs
