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

aws ec2 describe-instances --region "$REGION" --instance-ids ${INSTANCE_IDS} \
  --query 'Reservations[].Instances[].[InstanceId,State.Name,PublicIpAddress,PrivateIpAddress]' \
  --output table
