#!/usr/bin/env bash
# check-spot-pricing.sh — Session B spot-vs-on-demand pre-check
#
# Run this BEFORE `eksctl create cluster -f deploy/eks/cluster.yaml` so
# the session records the pricing decision in GATE2-ACCEPTANCE-PLAN.md
# § 12.6 before any billable resources spin up. This script only reads
# — it never provisions.
#
# What it does:
#   1. Pulls last-24h spot price history for each Session B instance
#      type in us-east-1a (i4i.2xlarge, i3en.3xlarge, m7i.xlarge).
#   2. Computes avg + max spot price per instance type.
#   3. Compares against the on-demand baseline embedded below (verify
#      against the AWS pricing page before any session where the number
#      matters — these are us-east-1 Linux on-demand as of 2026-04).
#   4. Emits a decision line per instance type (SPOT / ON-DEMAND) using
#      a 70 % premium threshold — if spot is cheaper than 70 % of
#      on-demand, use spot; otherwise the interruption risk isn't worth
#      the marginal saving on a 6-hour one-shot session.
#   5. Prints a 6-hour total cost estimate for the winning mix, and
#      flags the $50 hard-cap status per GATE2-ACCEPTANCE-PLAN.md
#      § 12.3.
#
# Requires: aws CLI v2 with EC2 read permissions. Does NOT require EKS
# permissions or a configured cluster.
#
# Usage:
#   ./deploy/eks/check-spot-pricing.sh                    # default region us-east-1, AZ us-east-1a
#   AWS_REGION=us-east-2 AZ=us-east-2b ./deploy/eks/check-spot-pricing.sh
#   THRESHOLD=0.6 ./deploy/eks/check-spot-pricing.sh      # stricter: spot must be <=60% of on-demand

set -euo pipefail

REGION="${AWS_REGION:-us-east-1}"
AZ="${AZ:-us-east-1a}"
THRESHOLD="${THRESHOLD:-0.70}"         # spot/on-demand ratio at which we still choose spot
HARD_CAP="${HARD_CAP:-50}"             # USD, per GATE2-ACCEPTANCE-PLAN § 12.3
SESSION_HOURS="${SESSION_HOURS:-6}"    # 6-hr weekend window

# Instance types mirror deploy/eks/cluster.yaml. Keep counts in sync if
# cluster.yaml changes.
#
# Format: instance_type:count:on_demand_hourly_usd:role
INSTANCES=(
    "i4i.2xlarge:3:0.686:aeon-pool"
    "i3en.3xlarge:3:1.248:redpanda-pool"
    "m7i.xlarge:1:0.2016:default-pool"
)

# Flat EKS control plane fee, per AWS (us-east-1).
EKS_CONTROL_PLANE_HOURLY="0.10"

# AWS CLI preflight.
if ! command -v aws >/dev/null 2>&1; then
    echo "ERROR: aws CLI not found on PATH. Install AWS CLI v2 and re-run." >&2
    exit 2
fi
if ! aws sts get-caller-identity >/dev/null 2>&1; then
    echo "ERROR: aws sts get-caller-identity failed. Check AWS_PROFILE / credentials." >&2
    exit 2
fi

printf "==> Session B spot-vs-on-demand pre-check\n"
printf "    Region:        %s\n" "$REGION"
printf "    AZ:            %s\n" "$AZ"
printf "    Threshold:     spot <= %s%% of on-demand => choose SPOT\n" "$(awk "BEGIN{printf \"%.0f\", $THRESHOLD*100}")"
printf "    Hard cap:      \$%s for %s-hour window\n" "$HARD_CAP" "$SESSION_HOURS"
printf "    Reference:     GATE2-ACCEPTANCE-PLAN.md § 12.3\n\n"

# 24-hour window for history lookback (AWS default is 90 days, but
# recent is most indicative of now).
start_time="$(date -u -d '24 hours ago' +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
    || date -u -v-24H +'%Y-%m-%dT%H:%M:%SZ')"

total_hourly_winning="0"
any_on_demand_fallback="no"

printf "%-18s %-16s %-10s %-10s %-10s %-9s %s\n" \
    "INSTANCE_TYPE" "ROLE" "ON_DEMAND" "SPOT_AVG" "SPOT_MAX" "RATIO" "DECISION"
printf "%-18s %-16s %-10s %-10s %-10s %-9s %s\n" \
    "------------------" "----------------" "----------" "----------" "----------" "---------" "--------"

for entry in "${INSTANCES[@]}"; do
    itype="$(cut -d: -f1 <<<"$entry")"
    count="$(cut -d: -f2 <<<"$entry")"
    on_demand="$(cut -d: -f3 <<<"$entry")"
    role="$(cut -d: -f4 <<<"$entry")"

    # describe-spot-price-history returns up to N recent samples in the
    # AZ. Empty output => no spot capacity / no recent history — treat
    # as ON-DEMAND fallback, not an error.
    history_json="$(aws ec2 describe-spot-price-history \
        --region "$REGION" \
        --instance-types "$itype" \
        --product-descriptions "Linux/UNIX" \
        --availability-zone "$AZ" \
        --start-time "$start_time" \
        --max-results 100 \
        --output json 2>/dev/null || echo '{"SpotPriceHistory":[]}')"

    sample_count="$(printf '%s' "$history_json" | awk -F'"SpotPrice"' 'NF>1{n+=NF-1} END{print n+0}')"

    if [ "$sample_count" -eq 0 ]; then
        decision="ON-DEMAND (no recent spot history)"
        effective="$on_demand"
        any_on_demand_fallback="yes"
        spot_avg="n/a"
        spot_max="n/a"
        ratio="n/a"
    else
        # Parse SpotPrice values — one per sample line. Using awk to
        # avoid a jq dep; the CLI already returns deterministic JSON.
        prices="$(printf '%s' "$history_json" \
            | awk 'BEGIN{RS=","} /SpotPrice/ {gsub(/[^0-9.]/,""); if (length($0)) print $0}')"

        spot_avg="$(printf '%s\n' "$prices" | awk '{s+=$1; n++} END{if(n) printf "%.4f", s/n; else print "0"}')"
        spot_max="$(printf '%s\n' "$prices" | awk 'BEGIN{m=0} {if($1>m) m=$1} END{printf "%.4f", m}')"
        ratio="$(awk "BEGIN{printf \"%.2f\", $spot_avg/$on_demand}")"

        # Decision rule: use SPOT iff avg <= threshold * on-demand AND
        # max <= 0.95 * on-demand (no benefit if the ceiling is close
        # to on-demand — interruption risk wins).
        cmp_avg="$(awk "BEGIN{print ($spot_avg <= $THRESHOLD * $on_demand) ? 1 : 0}")"
        cmp_max="$(awk "BEGIN{print ($spot_max <= 0.95 * $on_demand) ? 1 : 0}")"

        if [ "$cmp_avg" = "1" ] && [ "$cmp_max" = "1" ]; then
            decision="SPOT"
            effective="$spot_avg"
        else
            decision="ON-DEMAND"
            effective="$on_demand"
            any_on_demand_fallback="yes"
        fi
    fi

    line_hourly="$(awk "BEGIN{printf \"%.4f\", $effective * $count}")"
    total_hourly_winning="$(awk "BEGIN{printf \"%.4f\", $total_hourly_winning + $line_hourly}")"

    printf "%-18s %-16s \$%-9s \$%-9s \$%-9s %-9s %s\n" \
        "$itype" "$role" "$on_demand" "$spot_avg" "$spot_max" "$ratio" "$decision"
done

# Control plane + misc data transfer (per § 12.3 cost envelope).
total_with_control="$(awk "BEGIN{printf \"%.4f\", $total_hourly_winning + $EKS_CONTROL_PLANE_HOURLY + 0.20}")"
total_session="$(awk "BEGIN{printf \"%.2f\", $total_with_control * $SESSION_HOURS}")"
cap_margin="$(awk "BEGIN{printf \"%.2f\", $HARD_CAP - $total_session}")"
cap_ok="$(awk "BEGIN{print ($total_session <= $HARD_CAP) ? 1 : 0}")"

printf "\n==> Cost rollup for the winning mix\n"
printf "    Compute per hour:     \$%s\n" "$total_hourly_winning"
printf "    EKS control plane:    \$%s\n" "$EKS_CONTROL_PLANE_HOURLY"
printf "    Data transfer (est):  \$0.20\n"
printf "    Total per hour:       \$%s\n" "$total_with_control"
printf "    %s-hour window:        \$%s\n" "$SESSION_HOURS" "$total_session"
printf "    Hard cap:             \$%s\n" "$HARD_CAP"

if [ "$cap_ok" = "1" ]; then
    printf "    Cap status:           OK (margin \$%s)\n" "$cap_margin"
else
    printf "    Cap status:           BREACH (over by \$%s) -- DO NOT PROVISION\n" "$(awk "BEGIN{printf \"%.2f\", -1*$cap_margin}")"
fi

printf "\n==> Action\n"
if [ "$cap_ok" != "1" ]; then
    printf "    [X] Window does not fit the \$%s cap at the current mix.\n" "$HARD_CAP"
    printf "        Either shorten SESSION_HOURS, drop a pool node, or defer Session B.\n"
    exit 3
fi

if [ "$any_on_demand_fallback" = "yes" ]; then
    printf "    [!] At least one pool fell back to on-demand. Record this in\n"
    printf "        GATE2-ACCEPTANCE-PLAN.md § 12.6 pre-flight row before provisioning.\n"
else
    printf "    [v] All pools eligible for spot at the %s%% threshold.\n" \
        "$(awk "BEGIN{printf \"%.0f\", $THRESHOLD*100}")"
    printf "        Add 'spot: true' to matching nodeGroups in cluster.yaml if\n"
    printf "        spot is acceptable for this session (note: spot interruption\n"
    printf "        kills throughput runs mid-sweep -- default to on-demand unless\n"
    printf "        the saving is material).\n"
fi

printf "    [>] Record decision in docs/GATE2-ACCEPTANCE-PLAN.md § 12.6 pre-flight row.\n"
