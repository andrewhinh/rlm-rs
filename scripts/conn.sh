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

contains_value() {
  local needle="$1"
  shift
  local value
  for value in "$@"; do
    if [[ "$value" == "$needle" ]]; then
      return 0
    fi
  done
  return 1
}

SG_ID="$(aws ec2 describe-security-groups --region "$REGION" --group-names rlm-rs \
  --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)"
if [[ -z "${SG_ID}" || "${SG_ID}" == "None" ]]; then
  VPC_ID="$(aws ec2 describe-vpcs --region "$REGION" \
    --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)"
  SG_ID="$(aws ec2 create-security-group --region "$REGION" \
    --group-name rlm-rs --description "rlm-rs access" --vpc-id "$VPC_ID" \
    --query 'GroupId' --output text)"
fi

SSH_CIDR="${SSH_CIDR:-}"
if [[ -z "${SSH_CIDR}" ]]; then
  SSH_IP="$(curl -s https://checkip.amazonaws.com | tr -d '\n')"
  if [[ -n "${SSH_IP}" ]]; then
    SSH_CIDR="${SSH_IP}/32"
  fi
fi

if [[ -n "${SSH_CIDR}" ]]; then
  EXISTING_SSH_CIDRS="$(aws ec2 describe-security-groups --region "$REGION" \
    --group-ids "$SG_ID" \
    --query 'SecurityGroups[0].IpPermissions[?FromPort==`22` && ToPort==`22`].IpRanges[].CidrIp' \
    --output text)"
  EXISTING_SSH_CIDR_ARR=(${EXISTING_SSH_CIDRS})
  if ! contains_value "${SSH_CIDR}" "${EXISTING_SSH_CIDR_ARR[@]-}"; then
    aws ec2 authorize-security-group-ingress --region "$REGION" \
      --group-id "$SG_ID" --protocol tcp --port 22 --cidr "${SSH_CIDR}"
  fi
fi

APP_CIDR="${APP_CIDR:-$SSH_CIDR}"
if [[ -n "${APP_CIDR}" ]]; then
  EXISTING_APP_CIDRS="$(aws ec2 describe-security-groups --region "$REGION" \
    --group-ids "$SG_ID" \
    --query 'SecurityGroups[0].IpPermissions[?FromPort==`3000` && ToPort==`3000`].IpRanges[].CidrIp' \
    --output text)"
  EXISTING_APP_CIDR_ARR=(${EXISTING_APP_CIDRS})
  if ! contains_value "${APP_CIDR}" "${EXISTING_APP_CIDR_ARR[@]-}"; then
    aws ec2 authorize-security-group-ingress --region "$REGION" \
      --group-id "$SG_ID" --protocol tcp --port 3000 --cidr "${APP_CIDR}"
  fi
fi

if [[ -n "${INSTANCE_ID}" ]]; then
  INSTANCE_IDS="${INSTANCE_ID}"
else
  INSTANCE_IDS="$(aws ec2 describe-instances --region "$REGION" \
    --filters Name=tag:Name,Values=rlm-rs Name=instance-state-name,Values=running \
    --query 'Reservations[].Instances[].InstanceId' --output text)"
fi

if [[ -z "${INSTANCE_IDS}" || "${INSTANCE_IDS}" == "None" ]]; then
  echo "No running rlm-rs instances found"
  exit 0
fi

read -r INSTANCE_ID EXTRA <<< "${INSTANCE_IDS}"
if [[ -n "${EXTRA:-}" ]]; then
  echo "Multiple running rlm-rs instances found: ${INSTANCE_IDS}"
  echo "Specify one with instance-id arg"
  exit 1
fi

PUBLIC_IP="$(aws ec2 describe-instances --region "$REGION" --instance-ids "$INSTANCE_ID" \
  --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)"
rsync -av -e "ssh -i rlm-rs.pem" --exclude target --exclude .git . ubuntu@"$PUBLIC_IP":~/rlm-rs

ssh -i rlm-rs.pem ubuntu@"$PUBLIC_IP" -t 'sudo usermod -aG docker "$USER" && newgrp docker && docker ps && sudo chown -R "$USER":"$USER" ~/rlm-rs/target && chmod -R u+rwX ~/rlm-rs/target; exec bash -l'
