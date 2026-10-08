import { forwardRef, useEffect, useImperativeHandle, useRef } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { logs, type LogLine } from "@/state/logs";

export interface TerminalHandle {
  clear(): void;
  scrollToBottom(): void;
}

const ESC = "\x1b[";
const reset = `${ESC}0m`;
const color = {
  dim: `${ESC}38;2;103;115;135m`,
  protocol: `${ESC}38;2;125;211;252m`,
  protocolDim: `${ESC}38;2;84;120;150m`,
  error: `${ESC}38;2;248;113;113m`,
  warning: `${ESC}38;2;251;191;36m`,
  stderr: `${ESC}38;2;214;170;170m`,
};

// Output was already sanitized by the script runner and the engine; strip
// any escape sequence that still slipped through so nothing can drive the
// terminal.
// eslint-disable-next-line no-control-regex
const CONTROL = /\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b.|[\x00-\x08\x0b-\x1f\x7f]/g;

function render(line: LogLine, opts: { timestamps: boolean; protocol: boolean }): string | null {
  if (line.stream === "protocol" && !opts.protocol) return null;
  const text = line.text.replace(CONTROL, "");
  const time = opts.timestamps ? `${color.dim}${line.at.slice(11, 23)}${reset} ` : "";
  if (line.stream === "protocol") {
    const tone = line.level === "error" ? color.error : line.level === "warning" ? color.warning : line.level === "debug" ? color.protocolDim : color.protocol;
    return `${time}${tone}◆ ${text}${reset}`;
  }
  if (line.level === "error") return `${time}${color.error}${text}${reset}`;
  if (line.level === "warning") return `${time}${color.warning}${text}${reset}`;
  if (line.stream === "stderr") return `${time}${color.stderr}${text}${reset}`;
  return `${time}${text}`;
}

export const Terminal = forwardRef<
  TerminalHandle,
  { runId: string; follow: boolean; timestamps: boolean; protocol: boolean; header?: string; onUserScroll?: () => void }
>(function Terminal({ runId, follow, timestamps, protocol, header, onUserScroll }, ref) {
  const host = useRef<HTMLDivElement>(null);
  const term = useRef<XTerm | null>(null);
  const followRef = useRef(follow);
  followRef.current = follow;

  useImperativeHandle(ref, () => ({
    clear: () => term.current?.clear(),
    scrollToBottom: () => term.current?.scrollToBottom(),
  }));

  useEffect(() => {
    if (!host.current) return;
    const t = new XTerm({
      convertEol: true,
      disableStdin: true,
      cursorBlink: false,
      cursorStyle: "bar",
      cursorInactiveStyle: "none",
      fontFamily: '"JetBrains Mono Variable", ui-monospace, Menlo, monospace',
      fontSize: 12,
      lineHeight: 1.35,
      scrollback: 100_000,
      allowProposedApi: false,
      theme: {
        background: "#00000000",
        foreground: "#cfd7e3",
        cursor: "#00000000",
        selectionBackground: "#2dd4bf44",
        scrollbarSliderBackground: "#2b354488",
        scrollbarSliderHoverBackground: "#3a4658aa",
        scrollbarSliderActiveBackground: "#4a5870cc",
      },
      allowTransparency: true,
    });
    const fit = new FitAddon();
    t.loadAddon(fit);
    t.open(host.current);
    term.current = t;
    const doFit = () => {
      try {
        fit.fit();
      } catch {
        /* not visible */
      }
    };
    doFit();
    const observer = new ResizeObserver(doFit);
    observer.observe(host.current);

    const opts = { timestamps, protocol };
    const writeAll = (lines: LogLine[]) => {
      const out: string[] = [];
      for (const line of lines) {
        const s = render(line, opts);
        if (s !== null) out.push(s);
      }
      if (out.length) t.write(out.join("\r\n") + "\r\n", () => followRef.current && t.scrollToBottom());
    };
    const start = () => {
      t.reset();
      if (header) t.write(`${color.protocol}$ ${header}${reset}\r\n`);
      writeAll(logs.get(runId));
    };
    start();
    const unsubscribe = logs.subscribe(runId, (lines, isReset) => {
      if (isReset) start();
      else writeAll(lines);
    });
    const onScroll = t.onScroll(() => {
      const buffer = t.buffer.active;
      if (buffer.viewportY < buffer.baseY) onUserScroll?.();
    });
    return () => {
      unsubscribe();
      onScroll.dispose();
      observer.disconnect();
      t.dispose();
      term.current = null;
    };
  }, [runId, timestamps, protocol, header, onUserScroll]);

  useEffect(() => {
    if (follow) term.current?.scrollToBottom();
  }, [follow]);

  return <div ref={host} className="xterm-host h-full w-full overflow-hidden" />;
});
