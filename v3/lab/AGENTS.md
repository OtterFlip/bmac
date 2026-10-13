<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Agent instructions for the lab

These add to [`../AGENTS.md`](../AGENTS.md) for work in the nested lab.

## Experiments run in parallel

When you're finding out how a machine reacts to different parameters,
commands, or disk and boot states (an installer quirk, a boot path, firmware
or kernel options) and there are several possible explanations, test them
concurrently: one VM per theory, rather than one after another. Keep such a
batch to about 10 VMs at a time, and fewer when memory is short: before
starting one, check the workstation's available memory (`free -g`), count
the lab VMs already running (a host VM takes 8 GiB), and size the batch so
that at least a quarter of the workstation's memory stays free. An
experiment VM rarely needs a host VM's 8 GiB; give it only what the
experiment needs (an installer or a boot test runs in 2 to 4 GiB).

Use VMs made for the experiment, not the topology's `mox1`–`mox3`, the
appliance, or the QDevice, so a running scenario isn't disturbed, and remove
them, with any tailnet devices they registered, when the batch is done.
