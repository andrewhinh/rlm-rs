#!/usr/bin/env bash
set -euo pipefail

REGION=$(aws configure get region)

INSTANCE_IDS="$(aws ec2 describe-instances --region "$REGION" \
  --filters Name=tag:Name,Values=rlm-rs Name=instance-state-name,Values=pending,running,stopping,stopped \
  --query 'Reservations[].Instances[].InstanceId' --output text)"

if [[ -z "${INSTANCE_IDS}" ]]; then
  echo "No rlm-rs instances found"
  exit 0
fi

aws ec2 stop-instances --region "$REGION" --instance-ids ${INSTANCE_IDS}
aws ec2 wait --region "$REGION" instance-stopped --instance-ids ${INSTANCE_IDS}

echo "Stopped instances: ${INSTANCE_IDS}"
