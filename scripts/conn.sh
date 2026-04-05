#!/usr/bin/env bash
set -euo pipefail

INSTANCE_ID=""
REMOTE_CMD=""
SSH_OPTS=(-o StrictHostKeyChecking=accept-new)

usage() {
  echo "Usage: $(basename "$0") [options] [instance-id]"
  echo ""
  echo "Options:"
  echo "  --cmd <command>   Command to run non-interactively"
  echo "  -h, --help        Show this help message"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --cmd)
      if [[ $# -lt 2 ]]; then
        echo "Error: --cmd requires an argument"
        exit 1
      fi
      REMOTE_CMD="$2"
      shift 2
      ;;
    -*)
      echo "Unknown option: $1"
      usage
      exit 1
      ;;
    *)
      if [[ -z "${INSTANCE_ID}" ]]; then
        INSTANCE_ID="$1"
      else
        echo "Error: Too many arguments"
        usage
        exit 1
      fi
      shift
      ;;
  esac
done

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
rsync -av --delete -e "ssh ${SSH_OPTS[*]} -i rlm-rs.pem" --exclude target --exclude .git . ubuntu@"$PUBLIC_IP":~/rlm-rs

if [[ -n "${REMOTE_CMD}" ]]; then
  ssh "${SSH_OPTS[@]}" -i rlm-rs.pem ubuntu@"$PUBLIC_IP" bash -s -- "$REMOTE_CMD" << 'REMOTE_SCRIPT'
set -euo pipefail
REMOTE_CMD="$1"
REMOTE_CMD_PATH="$(mktemp /tmp/rlm-rs-cmd.XXXXXX.sh)"
trap 'rm -f "$REMOTE_CMD_PATH"' EXIT
cat > "$REMOTE_CMD_PATH" << 'REMOTE_CMD_HEADER'
#!/usr/bin/env bash
set -euo pipefail
REMOTE_CMD_HEADER
printf '%s\n' "$REMOTE_CMD" >> "$REMOTE_CMD_PATH"
chmod 700 "$REMOTE_CMD_PATH"
sudo usermod -aG docker "$USER"
sudo chown -R "$USER":"$USER" ~/rlm-rs/target 2>/dev/null || true
chmod -R u+rwX ~/rlm-rs/target 2>/dev/null || true
cd ~/rlm-rs
if command -v sg >/dev/null 2>&1; then
  sg docker -c "cd ~/rlm-rs && bash -lc $REMOTE_CMD_PATH"
else
  bash -lc "$REMOTE_CMD_PATH"
fi
REMOTE_SCRIPT
else
  ssh "${SSH_OPTS[@]}" -i rlm-rs.pem ubuntu@"$PUBLIC_IP" -t bash -s << 'REMOTE_SCRIPT'
sudo usermod -aG docker "$USER"
sudo chown -R "$USER":"$USER" ~/rlm-rs/target 2>/dev/null || true
chmod -R u+rwX ~/rlm-rs/target 2>/dev/null || true
cd ~/rlm-rs
if command -v sg >/dev/null 2>&1; then
  exec sg docker -c 'cd ~/rlm-rs && docker ps && exec bash -l'
fi
docker ps
exec bash -l
REMOTE_SCRIPT
fi
