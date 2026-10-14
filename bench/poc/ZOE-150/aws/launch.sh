#!/usr/bin/env bash
# Run the reduced run of ZOE-150 on a short-lived EC2 instance with local
# NVMe, copy the results back, and terminate the instance. Not for merge.
#
#   launch.sh --dry-run          print the cost estimate, launch nothing
#   launch.sh                    launch, wait, copy the results, terminate
#
# Options: --type (default c6id.4xlarge), --hours (the hours of the
# estimate, default 1), --commit (default HEAD, which must be on GitHub),
# --main-rev (default: the commit of main that the branch merged last), --out (default
# bench/poc/ZOE-150/results/aws), --ssh-cidr (the only source of SSH;
# default: the public address of this machine, /32).
#
# The region comes from AWS_REGION, else from the aws configuration, else
# eu-west-3. The script uses the credentials of the aws CLI and never
# writes them anywhere. Guard rails: a spot instance (on-demand only when
# spot fails), shutdown behaviour "terminate", a dead-man shutdown after 90
# minutes in the user data, the tag Project=zoe-spike on every resource, and
# a trap that terminates the instance and removes the key pair and the
# security group when this script ends, also on an error or Ctrl-C.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../../../.." && pwd)"
type=c6id.4xlarge
hours=1
dry=0
commit=""
main_rev=""
out="$repo_root/bench/poc/ZOE-150/results/aws"
cidr=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry=1 ;;
    --type) type=$2; shift ;;
    --hours) hours=$2; shift ;;
    --commit) commit=$2; shift ;;
    --main-rev) main_rev=$2; shift ;;
    --out) out=$2; shift ;;
    --ssh-cidr) cidr=$2; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

region="${AWS_REGION:-$(aws configure get region 2>/dev/null || true)}"
region="${region:-eu-west-3}"
export AWS_REGION="$region" AWS_PAGER=""
tag="Project=zoe-spike"
tags="ResourceType=instance,Tags=[{Key=Project,Value=zoe-spike},{Key=Name,Value=zoe-150}]"
tags_volume="ResourceType=volume,Tags=[{Key=Project,Value=zoe-spike}]"

say() { echo "[$(date +%H:%M:%S)] $*" >&2; }

# The cost estimate: the lowest spot price of the type in the region, and
# the on-demand price when the pricing API answers.
spot=$(aws ec2 describe-spot-price-history --instance-types "$type" \
  --product-descriptions "Linux/UNIX" --start-time "$(date -u +%Y-%m-%dT%H:%M:%S)" \
  --query 'sort_by(SpotPriceHistory,&SpotPrice)[0].[SpotPrice,AvailabilityZone]' --output text)
spot_price=${spot%%[[:space:]]*}
zone=${spot##*[[:space:]]}
ondemand=$(aws pricing get-products --region us-east-1 --service-code AmazonEC2 --filters \
  "Type=TERM_MATCH,Field=instanceType,Value=$type" "Type=TERM_MATCH,Field=regionCode,Value=$region" \
  "Type=TERM_MATCH,Field=operatingSystem,Value=Linux" "Type=TERM_MATCH,Field=tenancy,Value=Shared" \
  "Type=TERM_MATCH,Field=preInstalledSw,Value=NA" "Type=TERM_MATCH,Field=capacitystatus,Value=Used" \
  --query 'PriceList[0]' --output text 2>/dev/null \
  | python3 -c 'import json,sys
try:
    d = json.loads(sys.stdin.read())
    t = next(iter(d["terms"]["OnDemand"].values()))
    print(next(iter(t["priceDimensions"].values()))["pricePerUnit"]["USD"])
except Exception:
    print("unknown")' || echo unknown)
estimate=$(python3 -c "print(f'{float(\"$spot_price\") * float(\"$hours\"):.2f}')")
echo "region $region, type $type"
echo "spot price: $spot_price USD/h (lowest, in $zone); on-demand: $ondemand USD/h"
echo "estimate: $spot_price USD/h x $hours h = $estimate USD (spot), plus the root volume (40 GB gp3)"
[ "$dry" = 1 ] && exit 0

commit=$(git -C "$repo_root" rev-parse "${commit:-HEAD}")
git -C "$repo_root" fetch -q origin
main_rev=$(git -C "$repo_root" rev-parse "${main_rev:-$(git -C "$repo_root" merge-base "$commit" origin/main)}")
git -C "$repo_root" branch -r --contains "$commit" | grep -q . \
  || { echo "commit $commit is not on GitHub: push it first" >&2; exit 1; }

tmp=$(mktemp -d)
instance="" group="" key="zoe-150-$$"
cleanup() {
  set +e
  if [ -n "$instance" ]; then
    say "terminating $instance"
    aws ec2 terminate-instances --instance-ids "$instance" >/dev/null
    aws ec2 wait instance-terminated --instance-ids "$instance"
  fi
  if [ -n "$group" ]; then
    # The interface of the instance can hold the group for a short time.
    for _ in 1 2 3 4 5 6; do
      aws ec2 delete-security-group --group-id "$group" >/dev/null 2>&1 && break
      sleep 10
    done
  fi
  aws ec2 delete-key-pair --key-name "$key" >/dev/null 2>&1
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# A key pair and a security group for this run only: SSH from this address.
ssh-keygen -q -t ed25519 -N "" -f "$tmp/key"
aws ec2 import-key-pair --key-name "$key" --public-key-material "fileb://$tmp/key.pub" \
  --tag-specifications "ResourceType=key-pair,Tags=[{Key=Project,Value=zoe-spike}]" >/dev/null
vpc=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
group=$(aws ec2 create-security-group --group-name "zoe-150-$$" --description "ZOE-150 spike, SSH only" \
  --vpc-id "$vpc" --tag-specifications "ResourceType=security-group,Tags=[{Key=Project,Value=zoe-spike}]" \
  --query GroupId --output text)
cidr="${cidr:-$(curl -fsS https://checkip.amazonaws.com)/32}"
aws ec2 authorize-security-group-ingress --group-id "$group" --protocol tcp --port 22 --cidr "$cidr" >/dev/null

ami=$(aws ssm get-parameter --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
  --query Parameter.Value --output text)
sed -e "s|@COMMIT@|$commit|" -e "s|@MAIN_REV@|$main_rev|" \
  -e "s|@REPO@|https://github.com/adeweb-be/ZOE.git|" "$here/user-data.sh" >"$tmp/user-data.sh"

launch() {  # extra run-instances arguments...
  aws ec2 run-instances --image-id "$ami" --instance-type "$type" --count 1 \
    --key-name "$key" --security-group-ids "$group" \
    --instance-initiated-shutdown-behavior terminate \
    --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=40,VolumeType=gp3,DeleteOnTermination=true}' \
    --tag-specifications "$tags" "$tags_volume" \
    --user-data "file://$tmp/user-data.sh" "$@" \
    --query 'Instances[0].InstanceId' --output text
}
say "launching a spot $type with $commit"
instance=$(launch --instance-market-options \
  'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}' 2>"$tmp/spot.err") || {
  say "spot failed: $(tail -1 "$tmp/spot.err"); launching on demand"
  instance=$(launch)
}
say "instance $instance ($tag)"
aws ec2 wait instance-running --instance-ids "$instance"
ip=$(aws ec2 describe-instances --instance-ids "$instance" \
  --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
ssh_() { ssh -i "$tmp/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile="$tmp/known_hosts" \
  -o ConnectTimeout=10 -o LogLevel=ERROR "ubuntu@$ip" "$@"; }

# Wait for the run: at most 85 minutes, before the dead-man switch.
deadline=$(( $(date +%s) + 85 * 60 ))
status=""
while [ "$(date +%s)" -lt "$deadline" ]; do
  sleep 30
  status=$(ssh_ cat /var/lib/zoe150/status 2>/dev/null || true)
  last=$(ssh_ "tail -1 /mnt/nvme/zoe/bench/poc/ZOE-150/results/run.log 2>/dev/null || sudo tail -1 /var/lib/zoe150/user-data.log" 2>/dev/null || true)
  say "${status:-booting}: ${last:0:160}"
  case "$status" in done|failed) break ;; esac
done

mkdir -p "$out"
scp -q -r -i "$tmp/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile="$tmp/known_hosts" -o LogLevel=ERROR \
  "ubuntu@$ip:/mnt/nvme/zoe/bench/poc/ZOE-150/results/." "$out/" || say "no results to copy"
ssh_ sudo cat /var/lib/zoe150/user-data.log >"$out/user-data.log" 2>/dev/null || true
say "results in $out; status ${status:-unknown}"
[ "$status" = done ]
