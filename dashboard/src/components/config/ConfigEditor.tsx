import { forwardRef, useEffect, useImperativeHandle, useRef } from "react";
import { Compartment, EditorState, type Extension } from "@codemirror/state";
import {
  EditorView,
  crosshairCursor,
  drawSelection,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
  rectangularSelection,
} from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab, redo, redoDepth, undo, undoDepth } from "@codemirror/commands";
import { HighlightStyle, StreamLanguage, bracketMatching, syntaxHighlighting } from "@codemirror/language";
import { highlightSelectionMatches, openSearchPanel, search, searchKeymap } from "@codemirror/search";
import { unifiedMergeView } from "@codemirror/merge";
import { shell } from "@codemirror/legacy-modes/mode/shell";
import { tags as t } from "@lezer/highlight";

export interface EditorStatus {
  dirty: boolean;
  canUndo: boolean;
  canRedo: boolean;
  line: number;
  column: number;
  lines: number;
}

export interface ConfigEditorHandle {
  text(): string;
  undo(): void;
  redo(): void;
  find(): void;
  focus(): void;
  /** Treat TEXT as the saved state, keeping undo history. */
  markSaved(text: string): void;
}

const theme = EditorView.theme(
  {
    "&": { height: "100%", backgroundColor: "transparent", color: "var(--color-fg)", fontSize: "12.5px" },
    "&.cm-focused": { outline: "none" },
    ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.65", overflow: "auto" },
    ".cm-content": { padding: "10px 0", caretColor: "var(--color-accent)" },
    ".cm-line": { padding: "0 16px 0 12px" },
    ".cm-gutters": {
      backgroundColor: "var(--color-sunken)",
      color: "var(--color-fg-subtle)",
      borderRight: "1px solid var(--color-line)",
      minWidth: "52px",
    },
    ".cm-lineNumbers .cm-gutterElement": { padding: "0 10px 0 14px", fontSize: "11.5px" },
    ".cm-activeLine": { backgroundColor: "rgb(255 255 255 / 0.025)" },
    ".cm-activeLineGutter": { backgroundColor: "rgb(255 255 255 / 0.04)", color: "var(--color-fg-muted)" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--color-accent)", borderLeftWidth: "2px" },
    "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, ::selection": {
      backgroundColor: "color-mix(in oklab, var(--color-accent) 26%, transparent) !important",
    },
    ".cm-selectionMatch": { backgroundColor: "color-mix(in oklab, var(--color-info) 18%, transparent)" },
    ".cm-searchMatch": { backgroundColor: "color-mix(in oklab, var(--color-warn) 25%, transparent)", outline: "1px solid color-mix(in oklab, var(--color-warn) 45%, transparent)" },
    ".cm-searchMatch.cm-searchMatch-selected": { backgroundColor: "color-mix(in oklab, var(--color-warn) 45%, transparent)" },
    ".cm-panels": { backgroundColor: "var(--color-surface-2)", color: "var(--color-fg)", borderColor: "var(--color-line)" },
    ".cm-panels.cm-panels-top": { borderBottom: "1px solid var(--color-line)" },
    ".cm-panel.cm-search": { padding: "8px 12px", fontFamily: "var(--font-sans)", fontSize: "12.5px" },
    ".cm-panel.cm-search input, .cm-panel.cm-search button": {
      fontFamily: "var(--font-sans)",
      fontSize: "12px",
      borderRadius: "6px",
      border: "1px solid var(--color-line-strong)",
      backgroundColor: "var(--color-surface-3)",
      color: "var(--color-fg)",
      padding: "3px 8px",
      backgroundImage: "none",
    },
    ".cm-panel.cm-search input:focus": { outline: "1px solid var(--color-accent)" },
    ".cm-panel.cm-search label": { color: "var(--color-fg-muted)" },
    ".cm-panel.cm-search [name=close]": { border: "none", backgroundColor: "transparent", color: "var(--color-fg-subtle)", fontSize: "16px" },
    // The compare view: what the operator changed relative to the example.
    ".cm-changedLine": { backgroundColor: "color-mix(in oklab, var(--color-accent) 9%, transparent) !important" },
    ".cm-changedText": { background: "color-mix(in oklab, var(--color-accent) 28%, transparent) !important" },
    ".cm-deletedChunk": { backgroundColor: "color-mix(in oklab, var(--color-danger) 10%, transparent) !important", padding: "0 !important" },
    ".cm-deletedChunk .cm-deletedLine, .cm-deletedChunk del": { color: "color-mix(in oklab, var(--color-danger) 80%, var(--color-fg))", textDecoration: "none" },
    ".cm-insertedLine, .cm-insertedLine ins": { textDecoration: "none" },
    ".cm-changeGutter": { width: "3px", paddingLeft: "0" },
    ".cm-changedLineGutter": { backgroundColor: "var(--color-accent)" },
    ".cm-deletedLineGutter": { backgroundColor: "var(--color-danger)" },
    ".cm-chunkButtons": { display: "none" },
  },
  { dark: true },
);

const highlight = HighlightStyle.define([
  { tag: t.comment, color: "#5f6b7e", fontStyle: "italic" },
  { tag: [t.variableName, t.definition(t.variableName), t.propertyName], color: "#7dd3fc" },
  { tag: [t.string, t.special(t.string)], color: "#86efac" },
  { tag: [t.number, t.bool, t.atom], color: "#fcd34d" },
  { tag: [t.keyword, t.controlKeyword, t.operatorKeyword], color: "#c4b5fd" },
  { tag: [t.operator, t.punctuation, t.bracket], color: "#9ba7b9" },
  { tag: t.meta, color: "#f0abfc" },
]);

const shellLanguage = StreamLanguage.define(shell);

/** A small, keyboard-friendly editor for KEY=value config files. */
export const ConfigEditor = forwardRef<
  ConfigEditorHandle,
  {
    name: string;
    /** Changing the document key replaces the text and clears undo history. */
    docKey: string;
    initial: string;
    readOnly: boolean;
    /** When set, the editor shows changes relative to this text. */
    compareWith: string | null;
    onStatus(status: EditorStatus): void;
    onSave(): void;
  }
>(function ConfigEditor({ name, docKey, initial, readOnly, compareWith, onStatus, onSave }, ref) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const baseline = useRef(initial);
  const readOnlyCompartment = useRef(new Compartment());
  const compareCompartment = useRef(new Compartment());
  const callbacks = useRef({ onStatus, onSave });
  callbacks.current = { onStatus, onSave };

  const report = (v: EditorView) => {
    const head = v.state.selection.main.head;
    const line = v.state.doc.lineAt(head);
    callbacks.current.onStatus({
      dirty: v.state.doc.toString() !== baseline.current,
      canUndo: undoDepth(v.state) > 0,
      canRedo: redoDepth(v.state) > 0,
      line: line.number,
      column: head - line.from + 1,
      lines: v.state.doc.lines,
    });
  };

  const readOnlyExt = (on: boolean): Extension => [EditorState.readOnly.of(on), EditorView.editable.of(!on)];
  const compareExt = (original: string | null): Extension =>
    original === null ? [] : unifiedMergeView({ original, mergeControls: false, highlightChanges: true, gutter: true });

  useEffect(() => {
    if (!host.current) return;
    baseline.current = initial;
    const state = EditorState.create({
      doc: initial,
      extensions: [
        lineNumbers(),
        highlightActiveLineGutter(),
        highlightSpecialChars(),
        history(),
        drawSelection(),
        rectangularSelection(),
        crosshairCursor(),
        highlightActiveLine(),
        bracketMatching(),
        highlightSelectionMatches(),
        search({ top: true }),
        shellLanguage,
        syntaxHighlighting(highlight),
        theme,
        EditorView.lineWrapping,
        keymap.of([
          { key: "Mod-s", preventDefault: true, run: () => (callbacks.current.onSave(), true) },
          ...defaultKeymap,
          ...historyKeymap,
          ...searchKeymap,
          indentWithTab,
        ]),
        readOnlyCompartment.current.of(readOnlyExt(readOnly)),
        compareCompartment.current.of(compareExt(compareWith)),
        EditorView.updateListener.of((update) => {
          if (update.docChanged || update.selectionSet || update.transactions.length) report(update.view);
        }),
        EditorView.contentAttributes.of({ "aria-label": `Contents of ${name}`, spellcheck: "false" }),
      ],
    });
    const v = new EditorView({ state, parent: host.current });
    view.current = v;
    report(v);
    return () => {
      v.destroy();
      view.current = null;
    };
    // The editor is rebuilt only for a different document.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [docKey]);

  useEffect(() => {
    view.current?.dispatch({ effects: readOnlyCompartment.current.reconfigure(readOnlyExt(readOnly)) });
  }, [readOnly]);

  useEffect(() => {
    view.current?.dispatch({ effects: compareCompartment.current.reconfigure(compareExt(compareWith)) });
  }, [compareWith]);

  useImperativeHandle(ref, () => ({
    text: () => view.current?.state.doc.toString() ?? "",
    undo: () => void (view.current && undo(view.current)),
    redo: () => void (view.current && redo(view.current)),
    find: () => void (view.current && openSearchPanel(view.current)),
    focus: () => view.current?.focus(),
    markSaved: (text) => {
      baseline.current = text;
      if (view.current) report(view.current);
    },
  }));

  return <div ref={host} className="selectable h-full min-h-0" />;
});
