import { forwardRef, type ButtonHTMLAttributes } from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const buttonVariants = cva(
  "inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded-md font-medium transition-[background-color,color,border-color,box-shadow,opacity] duration-150 disabled:pointer-events-none disabled:opacity-45 [&_svg]:size-3.5 [&_svg]:shrink-0 cursor-default select-none",
  {
    variants: {
      variant: {
        primary:
          "bg-accent text-accent-fg hover:bg-[color-mix(in_oklab,var(--color-accent)_88%,white)] shadow-[inset_0_1px_0_rgb(255_255_255/0.25),0_1px_2px_rgb(0_0_0/0.4)]",
        secondary:
          "bg-surface-3 text-fg border border-line-strong hover:bg-[color-mix(in_oklab,var(--color-surface-3)_80%,white_8%)] shadow-[inset_0_1px_0_rgb(255_255_255/0.04)]",
        ghost: "text-fg-muted hover:text-fg hover:bg-surface-3",
        outline: "border border-line-strong text-fg hover:bg-surface-2",
        danger:
          "bg-danger-strong text-white hover:bg-[color-mix(in_oklab,var(--color-danger-strong)_88%,white)] shadow-[inset_0_1px_0_rgb(255_255_255/0.2),0_1px_2px_rgb(0_0_0/0.4)]",
        "danger-outline": "border border-danger/40 text-danger hover:bg-danger/10",
        link: "text-accent hover:underline underline-offset-2 px-0 h-auto",
      },
      size: {
        sm: "h-7 px-2.5 text-[12.5px]",
        md: "h-8 px-3 text-[13px]",
        lg: "h-9 px-4 text-[13.5px]",
        icon: "size-7",
        "icon-sm": "size-6 [&_svg]:size-3",
      },
    },
    defaultVariants: { variant: "secondary", size: "md" },
  },
);

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement>, VariantProps<typeof buttonVariants> {}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(({ className, variant, size, type = "button", ...props }, ref) => (
  <button ref={ref} type={type} className={cn(buttonVariants({ variant, size }), className)} {...props} />
));
Button.displayName = "Button";
