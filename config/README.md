<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# `config/`: your cluster's runtime configuration

This directory holds the configuration BMAC reads when it deploys and
operates **your** cluster. It is runtime configuration for operators, not
development configuration for working on BMAC itself.

The tracked files here are only templates. Before running BMAC, copy each one
to its real name in this same directory and fill in your own values:

```bash
cp config/cluster_dot_conf config/cluster.conf
cp config/mox1_dot_conf    config/mox1.conf      # one moxN.conf per host
cp config/mox2_dot_conf    config/mox2.conf
cp config/secrets_dot_env  config/secrets.env
chmod 600 config/secrets.env
```

Your copies are Git-ignored and must be owned by you (or root). The scripts
refuse a `secrets.env` whose mode is not exactly `0600`, and any
world-writable `.conf`. Read the comments at the top of each template before
filling it in.

## Treat your copies as read-only

Once a file has been used to build the cluster, its values are baked into the
hosts and guests. Editing them afterwards does not change the cluster; it only
makes your configuration disagree with it.

- **`moxN.conf`**: one per Proxmox host. `scripts/user_callable/hosts/add_proxmox_host.sh`
  uses it to build that host. After that, BMAC never writes to it; the
  production and staging guest scripts only read its per-host VM limits
  (`MAX_PROD_VM_COUNT_ON_THIS_HOST`, `MAX_STAGING_VM_COUNT_ON_THIS_HOST`).
- **`cluster.conf`**: *mostly* read-only. The one exception is
  `PROXMOX_CONTROL_NODE`, which must change when the cluster's control node
  changes. When you remove the current control node with
  `scripts/user_callable/hosts/remove_proxmox_host.sh`, BMAC asks whether to
  make this edit for you. Any other workstation that runs the host scripts
  must make the same edit by hand.
- **`secrets.env`**: set once, before the cluster is built, and never
  edited after that.
