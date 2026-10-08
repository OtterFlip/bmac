import type { ReactNode } from "react";
import * as D from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { cn } from "@/lib/utils";

/** In-app modal dialog. The dashboard never uses OS dialogs. */
export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  icon,
  tone = "neutral",
  children,
  footer,
  className,
  dismissible = true,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  icon?: ReactNode;
  tone?: "neutral" | "danger" | "warn" | "accent";
  children?: ReactNode;
  footer?: ReactNode;
  className?: string;
  dismissible?: boolean;
}) {
  const ring = {
    neutral: "bg-surface-3 text-fg-muted border-line-strong",
    danger: "bg-danger/12 text-danger border-danger/30",
    warn: "bg-warn/12 text-warn border-warn/30",
    accent: "bg-accent/12 text-accent border-accent/30",
  }[tone];
  return (
    <D.Root open={open} onOpenChange={(next) => (dismissible || next ? onOpenChange(next) : undefined)}>
      <D.Portal>
        <D.Overlay className="fixed inset-0 z-50 bg-black/55 backdrop-blur-[3px] animate-fade-in" />
        <D.Content
          onEscapeKeyDown={(e) => !dismissible && e.preventDefault()}
          onPointerDownOutside={(e) => !dismissible && e.preventDefault()}
          className={cn(
            "fixed left-1/2 top-1/2 z-50 flex max-h-[86vh] w-[min(560px,92vw)] -translate-x-1/2 -translate-y-1/2 flex-col overflow-hidden rounded-xl border border-line-strong bg-surface shadow-pop animate-pop-in focus:outline-none",
            tone === "danger" && "border-danger/35",
            className,
          )}
        >
          {tone === "danger" && <div className="h-[3px] shrink-0 bg-gradient-to-r from-danger-strong via-danger to-danger-strong" />}
          <div className="flex items-start gap-3 px-5 pt-5 pb-3">
            {icon && (
              <div className={cn("mt-0.5 flex size-9 shrink-0 items-center justify-center rounded-lg border [&_svg]:size-[18px]", ring)}>
                {icon}
              </div>
            )}
            <div className="min-w-0 flex-1">
              <D.Title className="text-[15px] font-semibold tracking-[-0.01em] text-fg">{title}</D.Title>
              {description ? (
                <D.Description className="mt-1 text-[13px] leading-relaxed text-fg-muted">{description}</D.Description>
              ) : (
                <D.Description className="sr-only">{typeof title === "string" ? title : "Dialog"}</D.Description>
              )}
            </div>
            {dismissible && (
              <D.Close className="-mr-1 -mt-1 rounded-md p-1.5 text-fg-subtle hover:bg-surface-3 hover:text-fg" aria-label="Close">
                <X className="size-4" />
              </D.Close>
            )}
          </div>
          {children && <div className="min-h-0 flex-1 overflow-y-auto px-5 pb-4">{children}</div>}
          {footer && (
            <div className="flex shrink-0 items-center justify-end gap-2 border-t border-line bg-sunken/60 px-5 py-3">{footer}</div>
          )}
        </D.Content>
      </D.Portal>
    </D.Root>
  );
}
