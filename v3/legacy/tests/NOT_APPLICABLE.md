<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# v2 tests not carried over

v2 tests of behavior that v3 doesn't have, and why. Tests of scripts that
weren't copied (see `../README.md`) move with the code that replaces them.

| v2 test | Reason |
| --- | --- |
| `test_config_location.py` | The workstation's config-location pointer is gone; `bmacd` writes the config to `/run/bmac/config/` |
| `test_shared_libs.py`: `test_secret_values_are_shell_only_and_do_not_change_effective_hash`, and the `secrets.env` parts of `test_layered_loading_formulas_and_secret_redaction`, `test_reloading_clears_prior_host_and_secret_layers`, and `test_secret_mode_and_symlinks_fail_closed` | There is no `secrets.env`: secrets arrive as secret fields, which `test_prompted_secrets_remain_shell_only_even_with_allexport` still covers. Every config file must now be exactly 0600, as the secret file had to be |
| `test_shared_libs.py`: `test_known_hosts_trust_file_mode_is_enforced`, `test_mox_ssh_uses_explicit_trust_and_quotes_remote_argv` | `mox_ssh` no longer chooses a known-hosts file: it runs locally, or uses `/etc/bmac/ssh_config`, which `bmacd` writes with host keys pinned from pmxcfs (tested in `crates/bmacd/tests/settings.rs`). The quoting test is kept as `test_mox_ssh_uses_the_bmac_ssh_config_and_quotes_remote_argv` |
| `test_shared_libs.py`: the BSD (`stat -f`) cases of the `*_in_both_flavours` stat tests | v3 runs only on Proxmox hosts, which have GNU `stat`; the GNU cases remain |
| `test_haproxy_routes.py`: `test_host_setup_uses_local_fixed_ingress_and_generic_names`, and `add_proxmox_host.sh` in `test_shell_scripts_parse_and_sync_help_needs_no_secrets` | `add_proxmox_host.sh` isn't copied; its ingress setup moves to the host setup job (M7), which gets its own tests |
| `test_cluster_control.py`: the parallel slot-probe tests (`test_slots_are_probed_at_once`, `test_a_hung_probe_is_stopped_at_the_deadline`, `test_a_reachable_low_slot_does_not_wait_for_later_slots`, `test_lowest_reachable_slot_is_used`) | Scripts run on a cluster host and read membership there instead of probing every slot from a workstation; `test_membership_is_read_on_this_host_only` replaces them |
| `test_add_prod_vm.py`: `test_workstation_jump_ssh_setup_writes_strict_managed_alias` | The workstation SSH alias moves to the Deployer's Workstation SSH page (M6.9); the operation ends with a next step showing the guest's attested host key |
| `test_add_prod_vm.py`: `test_openssl_without_sha512_crypt_stops_before_config_or_ssh` | It guarded against macOS LibreSSL; hosts have OpenSSL 3, and `test_root_password_arrives_as_a_secret_field_and_only_its_hash_is_kept` exercises `openssl passwd -6` |
| `test_staging_vm.py`: `test_openssl_without_sha512_crypt_stops_before_config_or_ssh` | As for `test_add_prod_vm.py`; `test_root_password_arrives_as_a_secret_field_and_only_its_hash_is_kept` replaces it |
| `test_remove_staging_vm.py`: `test_managed_workstation_alias_removal_preserves_unrelated_config` | Scripts run on a cluster host and never touch a workstation's `~/.ssh`; the Deployer's Workstation SSH page owns those entries (M6.9), and `test_workstation_ssh_is_left_to_the_deployer` checks the script leaves them alone |
| `test_extend_prod_vm_disk.py`: `test_sparse_limit_uses_smallest_headroom_guest_free_and_overhead`, `test_sparse_limit_is_zero_when_guest_free_covers_headroom`, `test_reserved_limit_scales_by_refreservation_ratio`, `test_reserve_is_ceiling_and_headroom_floors_to_mib`, `test_negative_headroom_allows_nothing`, `test_invalid_allowance_inputs_fail_closed` | The script's own growth arithmetic is gone: it asks `bmac growth`, which runs the capacity function. The tests are ported in `crates/bmac-storage/src/capacity.rs` (with the 1.5% sparse overhead replaced by the function's per-block-size charge, and raidz inflation added); `test_growth_limit_comes_from_bmac_growth_over_every_placement_host` checks the script's call |
| `test_host_membership.py`: `test_new_control_node_must_be_a_remaining_online_member` | There is no control node to hand over: `cluster-membership`, held by `bmacd`, serializes membership changes. Removing the host the registry records as control node moves that record to the host running the removal (`test_the_control_record_moves_to_this_host_only_when_it_names_the_target`) |
| `test_qdevice_scripts.py`: `test_cluster_conf_value_is_replaced_in_place` | Scripts no longer edit a workstation's `config/cluster.conf`; the QDevice host and every other value are cluster settings changed on the Settings page |
| `test_qdevice_scripts.py`: the `ssh-keygen -R` assertions in `test_forced_removal_never_touches_the_qdevice_host` and `test_inaccessible_qdevice_needs_agreement_and_tailscale_removal` | There is no workstation `known_hosts` to clean: the hosts reach the QDevice, and a replacement's host key is confirmed by fingerprint (`QdeviceHostKeyTrustTest`) |
