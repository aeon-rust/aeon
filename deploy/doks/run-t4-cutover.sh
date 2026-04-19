#!/usr/bin/env bash
# T4: measure partition cutover latency. For 20 random partitions, request
# transfer to next owner and time until /cluster/status reflects new owner.
# Uses a single curlimages/curl pod with bash to keep latencies tight.
set -euo pipefail

LEADER="${LEADER:-aeon-1.aeon-headless.aeon.svc.cluster.local:4471}"
NS=aeon
N=${N:-20}

# Spin a long-lived pod with curl so we don't pay pod-cold-start per probe
POD="t4-probe-$RANDOM"
kubectl -n $NS run $POD --restart=Never --image=curlimages/curl --command -- sleep 600 >/dev/null
kubectl -n $NS wait --for=condition=Ready pod/$POD --timeout=60s >/dev/null

trap "kubectl -n $NS delete pod $POD --grace-period=0 --force >/dev/null 2>&1 || true" EXIT

run_curl() { kubectl -n $NS exec $POD -- curl -sS -m 10 "$@"; }

echo "# partition source target accepted_ms cutover_ms"
for i in $(seq 1 $N); do
    pid=$((RANDOM % 24))
    cs=$(run_curl http://$LEADER/api/v1/cluster/status 2>/dev/null)
    cur_owner=$(echo "$cs" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['partitions'][str($pid)]['owner'])" 2>/dev/null || echo "")
    if [ -z "$cur_owner" ]; then continue; fi
    target=$(( (cur_owner % 3) + 1 ))

    t0=$(date +%s%3N)
    resp=$(run_curl -X POST -H "Content-Type:application/json" -d "{\"target_node_id\":$target}" http://$LEADER/api/v1/cluster/partitions/$pid/transfer 2>/dev/null)
    t1=$(date +%s%3N)
    accepted=$((t1 - t0))

    # Poll until owner reflects target
    deadline=$((t0 + 5000))
    new_owner=$cur_owner
    while [ $(date +%s%3N) -lt $deadline ]; do
        cs2=$(run_curl http://$LEADER/api/v1/cluster/status 2>/dev/null)
        new_owner=$(echo "$cs2" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['partitions'][str($pid)]['owner'])" 2>/dev/null || echo "$cur_owner")
        if [ "$new_owner" = "$target" ]; then break; fi
    done
    t2=$(date +%s%3N)
    cutover=$((t2 - t0))

    printf "p=%d %d→%d accepted=%dms cutover=%dms (final_owner=%s)\n" "$pid" "$cur_owner" "$target" "$accepted" "$cutover" "$new_owner"
done
