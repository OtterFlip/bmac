# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Run by systemd-run as `sh -c SCRIPT sh PACKAGE DIR`: installs PACKAGE and
# leaves apt's output in DIR/log and its exit status in DIR/exit.
apt-get install -y --no-install-recommends --allow-downgrades \
  -o DPkg::Lock::Timeout=600 -o Dpkg::Options::=--force-confold \
  "$1" >"$2/log" 2>&1
echo $? >"$2/exit.partial"
mv "$2/exit.partial" "$2/exit"
