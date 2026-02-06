#!/usr/bin/env bash
set -euo pipefail

REGION=$(aws configure get region)

INSTANCE_IDS="$(aws ec2 describe-instances --region "$REGION" \
  --filters Name=tag:Name,Values=rlm-rs Name=instance-state-name,Values=stopped \
  --query 'Reservations[].Instances[].InstanceId' --output text)"

if [[ -z "${INSTANCE_IDS}" ]]; then
  echo "No stopped rlm-rs instances found"
  exit 0
fi

aws ec2 start-instances --region "$REGION" --instance-ids ${INSTANCE_IDS}
aws ec2 wait --region "$REGION" instance-running --instance-ids ${INSTANCE_IDS}

PUBLIC_IPS="$(aws ec2 describe-instances --region "$REGION" --instance-ids ${INSTANCE_IDS} \
  --query 'Reservations[].Instances[].PublicIpAddress' --output text)"

echo "Started instances: ${INSTANCE_IDS}"
echo "Public IPs: ${PUBLIC_IPS}"
