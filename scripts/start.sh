#!/usr/bin/env bash
set -euo pipefail

INSTANCE_ID=""

if [[ $# -gt 1 ]]; then
  echo "Usage: $(basename "$0") [instance-id]"
  exit 1
fi

if [[ $# -eq 1 ]]; then
  if [[ "$1" == "-h" || "$1" == "--help" ]]; then
    echo "Usage: $(basename "$0") [instance-id]"
    exit 0
  fi
  INSTANCE_ID="$1"
fi

REGION=$(aws configure get region)

if [[ -n "${INSTANCE_ID}" ]]; then
  INSTANCE_IDS="${INSTANCE_ID}"
else
  INSTANCE_IDS="$(aws ec2 describe-instances --region "$REGION" \
    --filters Name=tag:Name,Values=rlm-rs Name=instance-state-name,Values=stopped \
    --query 'Reservations[].Instances[].InstanceId' --output text)"
fi

if [[ -z "${INSTANCE_IDS}" || "${INSTANCE_IDS}" == "None" ]]; then
  echo "No stopped rlm-rs instances found"
  exit 0
fi

aws ec2 start-instances --region "$REGION" --instance-ids ${INSTANCE_IDS}
aws ec2 wait --region "$REGION" instance-running --instance-ids ${INSTANCE_IDS}

PUBLIC_IPS="$(aws ec2 describe-instances --region "$REGION" --instance-ids ${INSTANCE_IDS} \
  --query 'Reservations[].Instances[].PublicIpAddress' --output text)"

echo "Started instances: ${INSTANCE_IDS}"
echo "Public IPs: ${PUBLIC_IPS}"
