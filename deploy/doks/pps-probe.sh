#!/usr/bin/env bash
# pps-probe.sh — Pre-flight DO PPS cap probe (GATE2-ACCEPTANCE-PLAN.md § 3.6)
#
# Creates two throwaway g-8vcpu-32gb droplets in AMS3, runs iperf3 UDP
# small-packet both directions, destroys them. Total cost ~$0.14, wall
# clock ~30 min.
#
# Pass criteria:
#   >= 500K PPS sustained  -> cap not applied, proceed with full cluster
#   200K - 400K PPS        -> cap exists but higher than historical; document
#   <= 207K PPS            -> historical cap active; revisit plan (see § 3.6)
#
# Prerequisites: doctl authenticated, SSH key uploaded to DO account.
#
# Usage:
#   export DO_SSH_KEY_FINGERPRINT=<your key fingerprint from doctl compute ssh-key list>
#   ./pps-probe.sh

set -euo pipefail

: "${DO_SSH_KEY_FINGERPRINT:?set DO_SSH_KEY_FINGERPRINT to your uploaded SSH key fingerprint}"

REGION=ams3
SIZE=g-8vcpu-32gb
IMAGE=ubuntu-24-04-x64
TAG=aeon-pps-probe
RUN_ID=$(date +%Y%m%d-%H%M%S)

echo "[$(date +%H:%M:%S)] Creating 2x $SIZE in $REGION..."
doctl compute droplet create "aeon-pps-a-$RUN_ID" "aeon-pps-b-$RUN_ID" \
    --region "$REGION" --size "$SIZE" --image "$IMAGE" \
    --ssh-keys "$DO_SSH_KEY_FINGERPRINT" \
    --tag-names "$TAG" \
    --wait

IPS=$(doctl compute droplet list --tag-name "$TAG" --format PublicIPv4 --no-header)
A=$(echo "$IPS" | sed -n 1p)
B=$(echo "$IPS" | sed -n 2p)
echo "[$(date +%H:%M:%S)] Droplet A: $A"
echo "[$(date +%H:%M:%S)] Droplet B: $B"

echo "[$(date +%H:%M:%S)] Waiting 30s for SSH to come up..."
sleep 30

# Install iperf3 on both
for IP in "$A" "$B"; do
    ssh -o StrictHostKeyChecking=no root@"$IP" \
        'DEBIAN_FRONTEND=noninteractive apt-get update -q && apt-get install -y -q iperf3' \
        >/dev/null
done

echo "[$(date +%H:%M:%S)] Starting iperf3 server on B..."
ssh -f -o StrictHostKeyChecking=no root@"$B" 'iperf3 -s -D'

sleep 2
echo "[$(date +%H:%M:%S)] Running 60s UDP small-packet test A -> B..."
ssh -o StrictHostKeyChecking=no root@"$A" \
    "iperf3 -c $B -u -b 2G -l 64 -t 60 -J" \
    | tee "pps-probe-$RUN_ID-a-to-b.json"

echo "[$(date +%H:%M:%S)] Running 60s reverse B -> A..."
ssh -f -o StrictHostKeyChecking=no root@"$A" 'iperf3 -s -D'
sleep 2
ssh -o StrictHostKeyChecking=no root@"$B" \
    "iperf3 -c $A -u -b 2G -l 64 -t 60 -J" \
    | tee "pps-probe-$RUN_ID-b-to-a.json"

echo
echo "=== Results summary ==="
for F in "pps-probe-$RUN_ID-a-to-b.json" "pps-probe-$RUN_ID-b-to-a.json"; do
    PPS=$(jq '.end.sum.packets / .end.sum.seconds' "$F")
    LOSS=$(jq '.end.sum.lost_percent' "$F")
    echo "$F: $PPS pps, ${LOSS}% loss"
done

echo
echo "[$(date +%H:%M:%S)] Destroying probe droplets..."
doctl compute droplet delete --force "aeon-pps-a-$RUN_ID" "aeon-pps-b-$RUN_ID"

echo "[$(date +%H:%M:%S)] Done. Record results in GATE2-ACCEPTANCE-PLAN.md § 10 Results."
