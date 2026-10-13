#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Mock workflow that starts the protocol and exits 0 without a completed
# event, as if its runner had been killed.
printf '{"type":"protocol","protocol":"bmac-ui","version":2}\n'
printf '{"type":"log","stream":"stdout","level":"info","text":"about to vanish"}\n'
exit 0
