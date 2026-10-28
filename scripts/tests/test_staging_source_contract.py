#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""add_staging_vm.sh's verify_source_vm_contract: a prod-linked staging VM
clones production's replicated zvol, so a production VM still running from an
instant restore's overlay over its PBS backup is refused as a source."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "user_callable" / "guests" / "staging" / "add_staging_vm.sh"

LSBLK = json.dumps({"exited": 1, "exitcode": 0, "out-data": json.dumps({"blockdevices": [
    {"name": "/dev/sda", "type": "disk", "children": [
        {"name": "/dev/sda1", "type": "part", "fstype": "vfat", "mountpoints": ["/boot/efi"]},
        {"name": "/dev/sda2", "type": "part", "fstype": "ext4", "mountpoints": ["/"]}]}]})})


def config(scsi0: str) -> str:
    return "\n".join([
        "name: prod1", "bios: ovmf", "machine: q35", "agent: enabled=1,fstrim_cloned_disks=1",
        "tags: app-ha-production;app-ha-purpose-production",
        "net0: virtio=02:00:00:00:00:01,bridge=vmbr1,firewall=1",
        "efidisk0: local-zfs:vm-100-disk-0,efitype=4m,ms-cert=2023k,pre-enrolled-keys=1,size=1M",
        f"scsi0: {scsi0},discard=on,iothread=1,replicate=1,size=32G,ssd=1", ""])


class StagingSourceContractTest(unittest.TestCase):
    def check(self, scsi0: str, registered: str, zvol_paths: bool = True) -> subprocess.CompletedProcess[str]:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        run_dir = Path(temporary.name)
        (run_dir / "qm.conf").write_text(config(scsi0))
        (run_dir / "lsblk.json").write_text(LSBLK)
        path_prefix = "/dev/zvol/rpool/data" if zvol_paths else "/var/lib/vz/images/100"
        body = f"""
            node_exec() {{
              case "$2 ${{3:-}}" in
                "qm config") cat {run_dir}/qm.conf ;;
                "qm agent") : ;;
                "qm guest") cat {run_dir}/lsblk.json ;;
                "pvesm path") printf '{path_prefix}/%s\\n' "${{4#*:}}" ;;
                "uname -m") echo x86_64 ;;
                "pveversion ") echo pve-manager/9.2.2/abc ;;
                *) : ;;
              esac
            }}
            RUN_DIR={run_dir}
            SOURCE_PLACEMENT=(mox1 mox2); STAGING_NODE=mox2; SOURCE_OWNER=mox1; SOURCE_VMID=100
            SOURCE_NAME=prod1; SOURCE_VOLUME={registered}; SOURCE_PURPOSE=production
            PROD_VM_STORAGE=local-zfs; STAGING_VM_STORAGE=local-zfs
            PRODUCTION_VM_TAG=app-ha-production; PURPOSE_TAG_PREFIX=app-ha-purpose-
            verify_source_vm_contract
            echo "source ok $SOURCE_VOLUME"
        """
        return subprocess.run(["bash", "-c", f'APP_HA_STAGING_VM_SOURCE_ONLY=1 source "$1"\n{body}', "bash",
                               str(SCRIPT)], text=True, capture_output=True, check=False)

    def test_a_vm_on_its_zvol_is_a_source(self) -> None:
        completed = self.check("local-zfs:vm-100-disk-1", "local-zfs:vm-100-disk-1")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("source ok local-zfs:vm-100-disk-1", completed.stdout)

    def test_a_vm_still_on_its_instant_restore_overlay_is_refused(self) -> None:
        completed = self.check("bmac-overlay:100/vm-100-disk-1.qcow2", "local-zfs:vm-100-disk-1")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("still an instant restore's overlay", completed.stderr)

    def test_a_root_disk_that_is_not_a_zvol_is_refused(self) -> None:
        completed = self.check("local-zfs:vm-100-disk-1", "local-zfs:vm-100-disk-1", zvol_paths=False)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("is not a ZFS zvol", completed.stderr)


if __name__ == "__main__":
    unittest.main()
