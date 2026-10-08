#!/usr/bin/env bash
# Mock workflow for engine tests and frontend development: a validated input
# group, a confirmation, a manual action, phases, a plan, a result, and a
# next step.
set -euo pipefail
source "$(cd -- "$(dirname -- "$0")/.." && pwd -P)/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"

bmac_ui_step "Validate inputs"
echo "Checking the cluster"
bmac_ui_group_begin resources "VM resources" "Choose the size of the VM."
bmac_ui_group_add cores --id cores --label "vCPU cores" --type integer --default 2 --min 1 --max 16 --required
bmac_ui_group_add hosts --id hosts --label "Placement hosts" --type multiselect --min-selected 2 \
  --option mox1 mox1 --option mox2 mox2 --option mox3 mox3
while true; do
  bmac_ui_group_request
  ((cores % 2 == 0)) || bmac_ui_field_error cores "Use an even number of cores."
  bmac_ui_group_check && break
done
echo "cores=${cores} hosts=${hosts}"

bmac_ui_step "Review"
BMAC_UI_DRY_RUN=false
bmac_ui_plan_begin "Create VM" "What will change"
bmac_ui_plan_item create "VM 100" "Create a VM with ${cores} cores"
bmac_ui_plan_end
if ! bmac_ui_confirm --title "Create the VM?" --message "This creates VM 100." --severity warning --text GO; then
  echo "Declined"
  exit 1
fi

bmac_ui_step "Console step"
bmac_ui_manual_action --title "Unlock the disk" --instruction "Type the passphrase at the console."
echo "WARNING: something mildly odd" >&2

bmac_ui_step "Finish"
bmac_ui_result vmid:int 100 name prod9
bmac_ui_next_step "Inspect the VM." --command "diagnostics/show_prod_vm_state.sh prod9" \
  --workflow show_prod_vm_state --arg resource=prod9
bmac_ui_next_step "Unknown workflow is dropped." --workflow not_a_workflow
bmac_ui_step_done
echo "done"
