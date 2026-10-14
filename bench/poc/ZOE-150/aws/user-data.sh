#!/usr/bin/env bash
# The user data of the ZOE-150 instance. Not for merge.
#
# launch.sh replaces @COMMIT@, @MAIN_REV@ and @REPO@ before the launch. The
# script holds no credential: the repository is public and the instance
# needs no AWS access.
#
# It mounts the local NVMe, installs nix and devenv, clones the repository
# at the commit of the POC branch, builds, runs the reduced run, and writes
# /var/lib/zoe150/status: "running", then "done" or "failed".
set -euo pipefail

# The dead-man switch: the instance stops (and so terminates) after 90
# minutes, also when the launcher is gone.
shutdown -h +90 "ZOE-150 dead-man switch"

state=/var/lib/zoe150
mkdir -p "$state"
exec >"$state/user-data.log" 2>&1
echo running >"$state/status"
trap 'echo failed >"$state/status"' ERR

# The local NVMe of the instance holds the repository, the builds and the
# stores of the run. The root volume holds only the system and /nix.
disk=""
for dev in /dev/nvme*n1; do
  if [ "$(cat "/sys/block/$(basename "$dev")/device/model" 2>/dev/null | xargs)" = "Amazon EC2 NVMe Instance Storage" ]; then
    disk=$dev
    break
  fi
done
[ -n "$disk" ] || { echo "no local NVMe"; false; }
mkfs.ext4 -q -F "$disk"
mkdir -p /mnt/nvme
mount -o noatime "$disk" /mnt/nvme
chown ubuntu:ubuntu /mnt/nvme

apt-get update -q
apt-get install -y -q git curl xz-utils

# nix, as the CI installs it (ci/src/ci/main.py), then the devenv release of
# devenv.yaml.
curl -fsSL https://releases.nixos.org/nix/nix-2.35.2/install -o /tmp/nix-install
sh /tmp/nix-install --daemon --yes
cat >>/etc/nix/nix.conf <<'EOF'
experimental-features = nix-command flakes
trusted-users = root ubuntu
extra-substituters = https://devenv.cachix.org
extra-trusted-public-keys = devenv.cachix.org-1:w1cLUi8dv3hnoSPGAuibQv+f9TZLr6cv/Hm9XgU50cw=
EOF
systemctl restart nix-daemon

cat >/home/ubuntu/zoe150.sh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
nix profile add github:cachix/devenv/v2.4.0
export PATH="$HOME/.nix-profile/bin:$PATH"
git clone -q @REPO@ /mnt/nvme/zoe
cd /mnt/nvme/zoe
git checkout -q @COMMIT@
devenv shell -- env MAIN_REV=@MAIN_REV@ bench/poc/ZOE-150/setup.sh
devenv shell -- bench/poc/ZOE-150/full.sh
EOF
chmod +x /home/ubuntu/zoe150.sh
sudo -u ubuntu -H /home/ubuntu/zoe150.sh
echo done >"$state/status"
