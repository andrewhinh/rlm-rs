#!/usr/bin/env bash
set -euo pipefail

REGION=$(aws configure get region)

INSTANCE_IDS="$(aws ec2 describe-instances --region "$REGION" \
  --filters Name=tag:Name,Values=rlm-rs Name=instance-state-name,Values=running \
  --query 'Reservations[].Instances[].InstanceId' --output text)"

if [[ -z "${INSTANCE_IDS}" ]]; then
  echo "No running rlm-rs instances found"
  exit 0
fi

aws ec2 reboot-instances --region "$REGION" --instance-ids ${INSTANCE_IDS}
aws ec2 wait --region "$REGION" instance-status-ok --instance-ids ${INSTANCE_IDS}

echo "Rebooted instances: ${INSTANCE_IDS}"
