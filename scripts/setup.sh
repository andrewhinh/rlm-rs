#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${IAM_USER:-}" ]]; then
  aws iam create-access-key --user-name "$IAM_USER"
fi

aws configure

REGION=$(aws configure get region)

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
  if [[ " ${EXISTING_SSH_CIDRS} " != *" ${SSH_CIDR} "* ]]; then
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
  if [[ " ${EXISTING_APP_CIDRS} " != *" ${APP_CIDR} "* ]]; then
    aws ec2 authorize-security-group-ingress --region "$REGION" \
      --group-id "$SG_ID" --protocol tcp --port 3000 --cidr "${APP_CIDR}"
  fi
fi

aws ec2 describe-key-pairs --region "$REGION" --key-names rlm-rs >/dev/null 2>&1 || \
  aws ec2 create-key-pair --region "$REGION" --key-name rlm-rs \
    --query "KeyMaterial" --output text > rlm-rs.pem

chmod 400 rlm-rs.pem
