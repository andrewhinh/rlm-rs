#!/usr/bin/env bash
set -euo pipefail

REGION=$(aws configure get region)

INSTANCE_ID="$(aws ec2 describe-instances --region "$REGION" \
  --filters Name=tag:Name,Values=rlm-rs Name=instance-state-name,Values=running \
  --query 'Reservations[0].Instances[0].InstanceId' --output text)"

if [[ -z "${INSTANCE_ID}" || "${INSTANCE_ID}" == "None" ]]; then
  echo "No running rlm-rs instances found"
  exit 0
fi

PUBLIC_IP="$(aws ec2 describe-instances --region "$REGION" --instance-ids "$INSTANCE_ID" \
  --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)"

rsync -av -e "ssh -i rlm-rs.pem" --exclude target --exclude .git . ubuntu@"$PUBLIC_IP":~/rlm-rs
ssh -i rlm-rs.pem ubuntu@"$PUBLIC_IP"
