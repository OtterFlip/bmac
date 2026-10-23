# Testing BMAC alerts by simulating failures

A hands-on test plan for the monitoring feature on the test cluster (mox1,
mox2, qdevice, prod1). Every test is a temporary break followed by the
command that reverses it. Run everything from the repo root on your dev workstation.

Not for production clusters: several tests briefly degrade redundancy, and a
few would cause an outage on a real cluster.

## Before you start

- `scripts/user_callable/diagnostics/show_monitoring_state.sh` should report
  everything up with no open problems.
- prod1's probe should be enabled (QDevice page, Health-check probes) with
  path `/api/v1/healthz`, status 200, and prod1's `guest.json` should watch
  `hello-todo.service` and `/etc/hello-todo/cert.pem` (Production page,
  Monitored services).
- Keep a second terminal following the agent's log on the node under test:

  ```bash
  ssh mox1 journalctl -fu app-ha-monitor      # or mox2, prod1
  ssh qdevice journalctl -fu app-ha-monitor
  ```

  Each problem the agent opens or resolves is one log line (`opened [p1]
  ...`, `resolved ...`), followed by `pulse ... sent`.

- Handy shell variables (paste into your dev worksation's shell):

  ```bash
  HOST_AGENT=/usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py
  NODE_AGENT=/usr/local/lib/app-ha-monitor/app_ha_monitor.py
  ```

### How long until the email arrives

| Kind of check | Opens after | Resolves after |
|---|---|---|
| Connectivity (links, quorum, QDevice vote, Tailscale, qnetd, cloudflared) | 3 failures, 30 s apart: about 90 s | 3 passes: about 90 s |
| Ingress probes | 3 failures, 60 s apart: about 3 min | about 3 min |
| Monitored guest services | 2 failures: about 60 s | 1 pass: about 30 s |
| Slow checks (ZFS, pool capacity, SMART, filesystems, certificates) | First run. They run every 30 min, so **restart the agent** to run them now | Next run (restart the agent again) |
| Dead man (node silent) | 60 s period + 180 s grace: about 4 min | First ping after it is back |

Pulses from one node are at least 60 s apart, so changes found together can
arrive in one email. Every pulse produces a DOWN email (`bmacevent`,
forwarded by your filter) and an UP email (`bmacclear`, deleted by F1). A
fix produces a pulse with a RESOLVED section, also forwarded.

Restarting the agent to run slow checks now:

```bash
ssh prod1 systemctl restart app-ha-monitor     # any node
```

A restart never re-announces problems the agent already knows about, but it
announces new ones and resolves fixed ones.

### Safety net for network tests

Every network test puts its rules in one nftables table, `inet bmac_test`,
and first schedules that table's removal, so a mistake undoes itself:

```bash
# On the node, before adding rules: remove the test table in 15 minutes no matter what.
systemd-run --on-active=15m --unit=bmac-test-revert nft delete table inet bmac_test
# Cancel the timer once you have undone the test by hand:
systemctl stop bmac-test-revert.timer
```

---

## 1. Dead-man switch: a node goes silent (G1)

Stopping the agent looks, from Healthchecks, exactly like the node dying or
losing its internet connection.

| Node | Break | Expect | Restore |
|---|---|---|---|
| prod1 | `ssh prod1 systemctl stop app-ha-monitor` | after about 4 min: `DOWN \| MyAppCloud bmaccheck prod1`, "success signal did not arrive on time", no BMAC tokens; forwarded by F2 (or F2a) | `ssh prod1 systemctl start app-ha-monitor`; expect `UP \| ...` with a heartbeat body, kept and forwarded |
| mox2 | `ssh mox2 systemctl stop app-ha-monitor` | same, for mox2 | `ssh mox2 systemctl start app-ha-monitor` |
| qdevice | `ssh qdevice systemctl stop app-ha-monitor` | same, for qdevice | `ssh qdevice systemctl start app-ha-monitor` |

While a check is down, Healthchecks' own reminders (if enabled in your
account) repeat hourly or daily.

## 2. Production app stops (G10, G5)

```bash
ssh prod1 systemctl stop hello-todo.service
```

Expect:

- after about 60 s, from prod1: `[p1] service:hello-todo.service  monitored
  service hello-todo.service is inactive`;
- after about 3 min, from qdevice: `[p1] ingress:prod1.example.com ...
  failed` (the app is down, so Cloudflare cannot reach it).

Restore, and expect RESOLVED pulses from both nodes:

```bash
ssh prod1 systemctl start hello-todo.service
```

## 3. Ingress probe fails while the app is up (G5)

Point the probe at a path that does not exist:

```bash
scripts/user_callable/qdevice/manage_qdevice_probes.sh --vm prod1 --enable \
  --path /api/v1/no-such-endpoint --status 200 --yes --save-if-probe-fails
```

Expect, after about 3 min, from qdevice: `[p1] ingress:<fqdn> ... returned
HTTP 404, expected 200`. Restore:

```bash
scripts/user_callable/qdevice/manage_qdevice_probes.sh --vm prod1 --enable \
  --path /api/v1/healthz --status 200 --yes
```

Variant: `--text something-that-is-not-in-the-body` with the right path
tests the expected-text match.

## 4. A host's tunnel replica stops (cloudflared)

```bash
ssh mox1 systemctl stop app-ha-cloudflared.service
```

Expect, after about 90 s, from mox1: `[p2] cloudflared  this host's
Cloudflare Tunnel replica is not ready (connection refused)`. Ingress keeps
working through mox2's replica, so there's no ingress alert. Restore:

```bash
ssh mox1 systemctl start app-ha-cloudflared.service
```

Optional, causes a short public outage for prod1: stop it on **both** hosts.
Expect qdevice's `ingress:` p1, both hosts' `cloudflared` p2, and
Cloudflare's own Tunnel Health Alert email (Down). Start both again.

## 5. Production VM's disk fills up (G10)

`fallocate` reserves space instantly without writing data, so the zvol
underneath does not actually grow. Fill / to about 5% free:

```bash
ssh prod1 'avail=$(df -B1 --output=avail / | tail -1); size=$(df -B1 --output=size / | tail -1);
  fallocate -l $((avail - size / 20)) /var/tmp/bmac-test-fill && df -h /'
ssh prod1 systemctl restart app-ha-monitor
```

Expect, at once: `[p2] disk:/  filesystem / 5.0% free (...), below 10%`.
Make it smaller than 5% free (use `size / 25`) and the item is p1 instead.

Restore (free space must rise above 12% to resolve, which it does):

```bash
ssh prod1 rm -f /var/tmp/bmac-test-fill
ssh prod1 systemctl restart app-ha-monitor
```

## 6. Certificate expiring soon, or unreadable (G10)

Create a certificate that expires in 2 days, and watch it:

```bash
ssh prod1 'openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=bmac-test \
  -keyout /root/bmac-test-key.pem -out /root/bmac-test-cert.pem 2>/dev/null'
scripts/user_callable/guests/prod/manage_prod_vm_monitoring.sh prod1 --add-cert /root/bmac-test-cert.pem --yes
ssh prod1 systemctl restart app-ha-monitor
```

Expect: `[p1] cert:/root/bmac-test-cert.pem  certificate ... expires in 2.0
days` (within 3 days is p1; within 14 days is p2; try `-days 10` for p2).

Unreadable: move the file away and restart the agent. Expect `[p2] cert:...
cannot be read`:

```bash
ssh prod1 mv /root/bmac-test-cert.pem /root/bmac-test-cert.pem.away
ssh prod1 systemctl restart app-ha-monitor
```

Restore:

```bash
scripts/user_callable/guests/prod/manage_prod_vm_monitoring.sh prod1 --remove-cert /root/bmac-test-cert.pem --yes
ssh prod1 rm -f /root/bmac-test-cert.pem /root/bmac-test-cert.pem.away /root/bmac-test-key.pem
ssh prod1 systemctl restart app-ha-monitor
```

## 7. A watched service that does not exist any more

```bash
ssh prod1 systemctl disable --now hello-todo.service
```

This behaves like test 2 (the unit is inactive), and survives reboots until
you `ssh prod1 systemctl enable --now hello-todo.service`.

## 8. QDevice loses its vote (G11)

Two-host cluster only. Both hosts stay up, so the cluster stays quorate.

```bash
ssh qdevice systemctl stop corosync-qnetd
```

Expect within about 2 min:

- from mox1 and mox2: `[p1] qdevice-vote  QDevice vote lost: the Qdevice
  contributes 0 votes; not voting on mox1, mox2 (qdevice client state:
  Connect failed)`;
- from qdevice: `[p1] qnetd:service  corosync-qnetd is inactive`.

Restore, and expect RESOLVED pulses from all three:

```bash
ssh qdevice systemctl start corosync-qnetd
```

## 9. One host's QDevice client stops

```bash
ssh mox2 systemctl stop corosync-qdevice
```

Expect: from qdevice `[p1] qnetd:mox2  mox2 is not connected to
corosync-qnetd`, and from the hosts a `qdevice-vote` item if their view of
the vote changes. Restore:

```bash
ssh mox2 systemctl start corosync-qdevice
```

## 10. Private VLAN link between the hosts fails (G3)

Blocks corosync's UDP traffic from mox2's VLAN address only, on mox1. Link1
(Tailscale) keeps the cluster together.

Check the addresses first with `ssh mox1 corosync-cfgtool -n`; the `LINK: 0`
line shows `10.213.0.11->10.213.0.12`.

```bash
ssh mox1 'systemd-run --on-active=15m --unit=bmac-test-revert nft delete table inet bmac_test
  nft add table inet bmac_test
  nft add chain inet bmac_test input "{ type filter hook input priority -10; }"
  nft add chain inet bmac_test output "{ type filter hook output priority -10; }"
  nft add rule inet bmac_test input ip saddr 10.213.0.12 udp dport 5405-5412 drop
  nft add rule inet bmac_test output ip daddr 10.213.0.12 udp dport 5405-5412 drop'
```

Expect, after about 90 s: from mox1 `[p2] link0:mox2  corosync link0
(private VLAN) to mox2 is DOWN`, and from mox2 `[p2] link0:mox1`. Restore:

```bash
ssh mox1 'nft delete table inet bmac_test; systemctl stop bmac-test-revert.timer'
```

## 11. Tailscale link between the hosts fails (G2)

The same, for mox2's Tailscale address. Find it with
`ssh mox2 tailscale ip -4`, and put it below instead of `MOX2_TS`.

```bash
MOX2_TS=$(ssh mox2 tailscale ip -4)
ssh mox1 "systemd-run --on-active=15m --unit=bmac-test-revert nft delete table inet bmac_test
  nft add table inet bmac_test
  nft add chain inet bmac_test input '{ type filter hook input priority -10; }'
  nft add chain inet bmac_test output '{ type filter hook output priority -10; }'
  nft add rule inet bmac_test input ip saddr $MOX2_TS udp dport 5405-5412 drop
  nft add rule inet bmac_test output ip daddr $MOX2_TS udp dport 5405-5412 drop"
```

Expect `[p2] link1:mox2` from mox1 and `[p2] link1:mox1` from mox2. Restore:

```bash
ssh mox1 'nft delete table inet bmac_test; systemctl stop bmac-test-revert.timer'
```

Do **not** combine tests 10 and 11. Losing both links splits the cluster,
and HA may fence (reboot) a host. The agent's p1 for "both links down" is
covered by automated tests.

## 12. QDevice cannot reach a host over Tailscale (G2)

Blocks only the QDevice's ICMP echo requests to mox1. qnetd (TCP) is not
affected.

```bash
MOX1_TS=$(ssh mox1 tailscale ip -4)
ssh qdevice "systemd-run --on-active=15m --unit=bmac-test-revert nft delete table inet bmac_test
  nft add table inet bmac_test
  nft add chain inet bmac_test output '{ type filter hook output priority -10; }'
  nft add rule inet bmac_test output ip daddr $MOX1_TS icmp type echo-request drop"
```

Expect, after about 90 s, from qdevice: `[p2] tailscale:mox1  ICMP over
Tailscale from the QDevice to mox1 (...) fails (tailscale online=true)`.

To test the "every host fails at once" p1, also add a rule for mox2's
address. Restore:

```bash
ssh qdevice 'nft delete table inet bmac_test; systemctl stop bmac-test-revert.timer'
```

## 13. A host loses its internet connection (G4)

Advanced, and the most realistic test. mox1's public interface stops
sending, so mox1 loses Healthchecks, Tailscale, and its tunnel replica,
while the private VLAN keeps the cluster together. Your SSH session to mox1
(over Tailscale) drops too, so the 10-minute auto-revert is what brings it
back. Run it while nothing important is happening on prod1.

```bash
# The public interface: the device of mox1's default route (for example vmbr0).
ssh mox1 'ip route get 1.1.1.1'
PUBLIC_IF=vmbr0    # set from the output above
ssh mox1 "systemd-run --on-active=10m --unit=bmac-test-revert nft delete table inet bmac_test
  nft add table inet bmac_test
  nft add chain inet bmac_test output '{ type filter hook output priority -10; }'
  nft add rule inet bmac_test output oifname $PUBLIC_IF drop"
```

Expect, in this order:

- from mox2, after about 90 s: `[p2] link1:mox1` (Tailscale) but **no**
  `link0:mox1` (VLAN), and `[p2] cloudflared` is *not* reported (that's
  mox1's own check, and mox1 cannot send);
- from qdevice: `[p2] tailscale:mox1`;
- after about 4 min: dead-man `DOWN | MyAppCloud bmaccheck mox1`.

Together these are the README's "host is alive but lost its internet
connection" row. Meanwhile prod1 stays reachable through mox2's replica, and
mox1's guests egress through mox2.

Restore by waiting for the 10-minute auto-revert, or from mox2 over the
VLAN:

```bash
ssh mox2 ssh mox1.pve.internal 'nft delete table inet bmac_test; systemctl stop bmac-test-revert.timer'
```

Expect mox1's UP email, and RESOLVED pulses from mox2 and qdevice.

## 14. Proxmox's own email is stuck (G9)

Postfix keeps Proxmox's mail in the deferred queue instead of sending it.
The alert fires when a message is older than
`MONITOR_POSTFIX_QUEUE_MAX_AGE_MINUTES` (default 60). To shorten the wait,
add `MONITOR_POSTFIX_QUEUE_MAX_AGE_MINUTES=10` to `config/cluster.conf` and
run `scripts/user_callable/hosts/update_cluster_runtime.sh` (agents re-read
their config every cycle; remove the line and rerun afterwards).

```bash
ssh mox1 "postconf -e 'defer_transports = smtp' && postfix reload &&
  printf 'Subject: BMAC queue test\n\nqueued on purpose\n' | sendmail root && postqueue -p"
```

Expect, once the message is older than the limit (within one more 30 s
cycle): `[p2] postfix-queue  1 message older than N minutes in the Postfix
queue (oldest ..., queue deferred, to <your domain>: deferred transport)`.

Restore (the test email is then delivered to you):

```bash
ssh mox1 'postconf -X defer_transports && postfix reload && postqueue -f'
```

## 15. Host pool space below 10% (G8)

A non-sparse zvol reserves its whole size at once without writing data.
It's outside Proxmox's storage (`rpool/data`), so nothing will use it. Leave
about 5% free:

```bash
ssh mox2 'avail=$(zfs get -Hp -o value available rpool); used=$(zfs get -Hp -o value used rpool);
  total=$((avail + used)); size=$(( (avail - total / 20) / 1073741824 ))G;
  echo "creating rpool/bmac-test-fill of $size"; zfs create -V "$size" rpool/bmac-test-fill && zfs list rpool'
ssh mox2 systemctl restart app-ha-monitor
```

Expect, at once: `[p2] capacity:rpool  rpool 5.x% free (...), below 10%`.
Below 5% it's p1. Replication of prod1 to mox2 may also fail while the
space is taken, and Proxmox may email about it.

Restore promptly:

```bash
ssh mox2 'zfs destroy rpool/bmac-test-fill && zfs list rpool'
ssh mox2 systemctl restart app-ha-monitor
```

## 16. Disks, ZFS faults, SMART (G6, G7)

These can't be faked safely on live drives; automated tests cover their
parsing and alerts, using the shapes captured from mox1.

- **ZFS fault (G7):** `scripts/user_callable/hosts/simulate_disk_failure_and_replacement.sh
  --host mox2` walks through a real mirror-member failure and replacement.
  During the failed phase, restart mox2's agent and expect `[p1] zfs:rpool
  pool rpool is DEGRADED (mirror-0: ... FAULTED)` (or `REMOVED`/`UNAVAIL`),
  plus Proxmox's own ZED email.
- **SMART (G6):** see what the agent reads today, without sending anything:

  ```bash
  ssh mox1 python3 -I $HOST_AGENT check --once | grep -E 'smart|nvme'
  ```

  The README's optional smartd email test checks Proxmox's own SMART email
  path.

## 17. Staging clones never alert

If a staging clone of prod1 exists (for example stage1prod1):

```bash
ssh stage1prod1 systemctl status app-ha-monitor
```

Expect the unit to show `ExecCondition` failed ("condition unmet"), never
running, and prod1's check unaffected. Create and remove a clone with the
Staging page to test it end to end.

## 18. Reminders

Leave one harmless problem open (for example test 6 with `-days 10`) for
`MONITOR_REMINDER_HOURS` (default 6). Expect a pulse with a REMINDER
section, and the problem's ONGOING line in the meantime in any other pulse
from that node.

## After testing

```bash
scripts/user_callable/diagnostics/show_monitoring_state.sh
for n in mox1 mox2 qdevice; do ssh $n 'nft list table inet bmac_test 2>/dev/null && echo "LEFTOVER on $(hostname)"'; done
ssh mox2 zfs list rpool/bmac-test-fill 2>/dev/null && echo "LEFTOVER zvol"
ssh prod1 ls /var/tmp/bmac-test-fill /root/bmac-test-cert.pem 2>/dev/null
ssh mox1 postconf defer_transports
```

Everything should be up with no open problems, and no leftovers should be
listed.
