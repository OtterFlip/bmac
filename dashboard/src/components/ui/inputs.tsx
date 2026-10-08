import { forwardRef, type InputHTMLAttributes, type ReactNode, type TextareaHTMLAttributes } from "react";
import * as RSwitch from "@radix-ui/react-switch";
import * as RCheckbox from "@radix-ui/react-checkbox";
import * as RSelect from "@radix-ui/react-select";
import { Check, ChevronDown } from "lucide-react";
import { cn } from "@/lib/utils";

const fieldBase =
  "w-full rounded-md border border-line-strong bg-sunken px-2.5 text-[13px] text-fg placeholder:text-fg-subtle/80 transition-[border-color,box-shadow] duration-150 hover:border-[color-mix(in_oklab,var(--color-line-strong)_70%,white_12%)] focus:border-accent/70 focus:outline-none focus:ring-[3px] focus:ring-accent/15 disabled:opacity-50 aria-invalid:border-danger/70 aria-invalid:focus:ring-danger/15";

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement> & { suffix?: ReactNode; mono?: boolean }>(
  ({ className, suffix, mono, ...props }, ref) => {
    const input = <input ref={ref} className={cn(fieldBase, "h-8", mono && "font-mono text-[12.5px]", suffix && "pr-12", className)} {...props} />;
    if (!suffix) return input;
    return (
      <div className="relative">
        {input}
        <span className="pointer-events-none absolute inset-y-0 right-2.5 flex items-center text-[12px] text-fg-subtle">{suffix}</span>
      </div>
    );
  },
);
Input.displayName = "Input";

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaHTMLAttributes<HTMLTextAreaElement>>(({ className, ...props }, ref) => (
  <textarea ref={ref} className={cn(fieldBase, "min-h-[84px] py-2 font-mono text-[12.5px] leading-relaxed", className)} {...props} />
));
Textarea.displayName = "Textarea";

export function Switch({
  checked,
  onCheckedChange,
  disabled,
  id,
  labelledBy,
  describedBy,
}: {
  checked: boolean;
  onCheckedChange: (v: boolean) => void;
  disabled?: boolean;
  id?: string;
  labelledBy?: string;
  describedBy?: string;
}) {
  return (
    <RSwitch.Root
      id={id}
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      checked={checked}
      onCheckedChange={onCheckedChange}
      disabled={disabled}
      className="relative inline-flex h-[18px] w-[32px] shrink-0 items-center rounded-full border border-line-strong bg-surface-3 transition-colors data-[state=checked]:border-accent/60 data-[state=checked]:bg-accent disabled:opacity-50"
    >
      <RSwitch.Thumb className="block size-[12px] translate-x-[2px] rounded-full bg-fg-muted shadow transition-transform duration-150 data-[state=checked]:translate-x-[16px] data-[state=checked]:bg-accent-fg" />
    </RSwitch.Root>
  );
}

export function Checkbox({
  checked,
  onCheckedChange,
  disabled,
  id,
}: {
  checked: boolean;
  onCheckedChange: (v: boolean) => void;
  disabled?: boolean;
  id?: string;
}) {
  return (
    <RCheckbox.Root
      id={id}
      checked={checked}
      onCheckedChange={(v) => onCheckedChange(v === true)}
      disabled={disabled}
      className="flex size-4 shrink-0 items-center justify-center rounded-[4px] border border-line-strong bg-sunken transition-colors data-[state=checked]:border-accent data-[state=checked]:bg-accent disabled:opacity-50"
    >
      <RCheckbox.Indicator>
        <Check className="size-3 text-accent-fg" strokeWidth={3} />
      </RCheckbox.Indicator>
    </RCheckbox.Root>
  );
}

export function Select({
  value,
  onValueChange,
  options,
  placeholder = "Choose…",
  disabled,
  invalid,
  id,
  labelledBy,
  describedBy,
}: {
  value: string | undefined;
  onValueChange: (v: string) => void;
  options: { value: string; label: string; help?: string }[];
  placeholder?: string;
  disabled?: boolean;
  invalid?: boolean;
  id?: string;
  labelledBy?: string;
  describedBy?: string;
}) {
  return (
    <RSelect.Root value={value || undefined} onValueChange={onValueChange} disabled={disabled}>
      <RSelect.Trigger id={id} aria-labelledby={labelledBy} aria-describedby={describedBy} aria-invalid={invalid || undefined} className={cn(fieldBase, "flex h-8 items-center justify-between gap-2 text-left data-[placeholder]:text-fg-subtle")}>
        <RSelect.Value placeholder={placeholder} />
        <RSelect.Icon>
          <ChevronDown className="size-3.5 text-fg-subtle" />
        </RSelect.Icon>
      </RSelect.Trigger>
      <RSelect.Portal>
        <RSelect.Content
          position="popper"
          sideOffset={4}
          className="z-[70] max-h-[min(360px,var(--radix-select-content-available-height))] min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-lg border border-line-strong bg-surface-2 shadow-pop animate-fade-in"
        >
          <RSelect.Viewport className="p-1">
            {options.map((o) => (
              <RSelect.Item
                key={o.value}
                value={o.value}
                className="relative flex cursor-default select-none flex-col rounded-md py-1.5 pl-7 pr-3 text-[13px] text-fg outline-none data-[highlighted]:bg-accent/12 data-[highlighted]:text-fg"
              >
                <RSelect.ItemIndicator className="absolute left-2 top-2">
                  <Check className="size-3.5 text-accent" />
                </RSelect.ItemIndicator>
                <RSelect.ItemText>{o.label}</RSelect.ItemText>
                {o.help && <span className="mt-0.5 text-[11.5px] text-fg-subtle">{o.help}</span>}
              </RSelect.Item>
            ))}
          </RSelect.Viewport>
        </RSelect.Content>
      </RSelect.Portal>
    </RSelect.Root>
  );
}

export function Label({ htmlFor, children, required, className }: { htmlFor?: string; children: ReactNode; required?: boolean; className?: string }) {
  return (
    <label htmlFor={htmlFor} className={cn("mb-1.5 block text-[12.5px] font-medium text-fg", className)}>
      {children}
      {required && <span className="ml-0.5 text-danger">*</span>}
    </label>
  );
}
