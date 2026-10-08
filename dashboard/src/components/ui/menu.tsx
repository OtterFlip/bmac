import type { ReactNode } from "react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { MoreHorizontal } from "lucide-react";
import { Button } from "./button";
import { cn } from "@/lib/utils";

export interface MenuItem {
  label: ReactNode;
  icon?: ReactNode;
  onSelect: () => void;
  danger?: boolean;
  disabled?: boolean;
  hint?: ReactNode;
}

export function Menu({ items, trigger, label = "Actions" }: { items: (MenuItem | "separator")[]; trigger?: ReactNode; label?: string }) {
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        {trigger ?? (
          <Button variant="ghost" size="icon-sm" aria-label={label} onClick={(e) => e.stopPropagation()}>
            <MoreHorizontal />
          </Button>
        )}
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={4}
          className="z-[70] min-w-[220px] rounded-lg border border-line-strong bg-surface-2 p-1 shadow-pop animate-fade-in"
          onClick={(e) => e.stopPropagation()}
        >
          {items.map((item, i) =>
            item === "separator" ? (
              <DropdownMenu.Separator key={i} className="my-1 h-px bg-line" />
            ) : (
              <DropdownMenu.Item
                key={i}
                disabled={item.disabled}
                onSelect={item.onSelect}
                className={cn(
                  "flex cursor-default items-center gap-2 rounded-md px-2 py-1.5 text-[12.5px] outline-none select-none data-[disabled]:opacity-45 [&_svg]:size-3.5",
                  item.danger ? "text-danger data-[highlighted]:bg-danger/12" : "text-fg data-[highlighted]:bg-surface-3",
                )}
              >
                <span className={cn("shrink-0", item.danger ? "text-danger" : "text-fg-subtle")}>{item.icon}</span>
                <span className="flex-1">{item.label}</span>
                {item.hint && <span className="text-[11px] text-fg-subtle">{item.hint}</span>}
              </DropdownMenu.Item>
            ),
          )}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
