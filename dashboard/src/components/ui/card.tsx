import type { HTMLAttributes, ReactNode } from "react";
import { cn } from "@/lib/utils";

export function Card({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn(
        "rounded-xl border border-line bg-surface shadow-[inset_0_1px_0_rgb(255_255_255/0.03)]",
        className,
      )}
      {...props}
    />
  );
}

export function CardHeader({
  title,
  subtitle,
  icon,
  actions,
  className,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  icon?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex items-center gap-3 border-b border-line px-4 py-3", className)}>
      {icon && <div className="text-fg-subtle [&_svg]:size-4">{icon}</div>}
      <div className="min-w-0 flex-1">
        <div className="text-[13px] font-semibold tracking-[-0.005em] text-fg">{title}</div>
        {subtitle && <div className="mt-0.5 truncate text-[12px] text-fg-subtle">{subtitle}</div>}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-1.5">{actions}</div>}
    </div>
  );
}

export function CardBody({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("p-4", className)} {...props} />;
}
