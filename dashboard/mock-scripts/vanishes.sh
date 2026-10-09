#!/usr/bin/env bash
# Mock workflow that starts the protocol and exits 0 without a completed
# event, as if its runner had been killed.
printf '{"type":"protocol","protocol":"bmac-ui","version":2}\n'
printf '{"type":"log","stream":"stdout","level":"info","text":"about to vanish"}\n'
exit 0
