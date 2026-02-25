#!/usr/bin/env bash
set -euo pipefail

REGION=$(aws configure get region)
ARCH="${ARCH:-arm64}"
ROOT_GB="${ROOT_GB:-50}"

if [[ "${ARCH}" == "arm64" ]]; then
  INSTANCE_TYPE="${INSTANCE_TYPE:-t4g.medium}"
  AMI_PARAM="/aws/service/canonical/ubuntu/server/24.04/stable/current/arm64/hvm/ebs-gp3/ami-id"
else
  INSTANCE_TYPE="${INSTANCE_TYPE:-t3.medium}"
  AMI_PARAM="/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
fi

AMI_ID="$(aws ssm get-parameters --region "$REGION" --names "$AMI_PARAM" \
  --query 'Parameters[0].Value' --output text)"
SG_ID="$(aws ec2 describe-security-groups --region "$REGION" --group-names rlm-rs \
  --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)"
if [[ -z "${SG_ID}" || "${SG_ID}" == "None" ]]; then
  VPC_ID="$(aws ec2 describe-vpcs --region "$REGION" \
    --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)"
  SG_ID="$(aws ec2 create-security-group --region "$REGION" \
    --group-name rlm-rs --description "rlm-rs access" --vpc-id "$VPC_ID" \
    --query 'GroupId' --output text)"
fi

INSTANCE_ID="$(aws ec2 run-instances --region "$REGION" \
  --image-id "$AMI_ID" \
  --instance-type "$INSTANCE_TYPE" \
  --key-name rlm-rs \
  --security-group-ids "$SG_ID" \
  --block-device-mappings "DeviceName=/dev/sda1,Ebs={VolumeSize=${ROOT_GB},VolumeType=gp3,DeleteOnTermination=true}" \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=rlm-rs}]' \
  --query 'Instances[0].InstanceId' --output text)"

aws ec2 wait --region "$REGION" instance-running --instance-ids "$INSTANCE_ID"

PUBLIC_IP="$(aws ec2 describe-instances --region "$REGION" --instance-ids "$INSTANCE_ID" \
  --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)"

echo "Instance: ${INSTANCE_ID}"
echo "Public IP: ${PUBLIC_IP}"
