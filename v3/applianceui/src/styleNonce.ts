// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The page's policy allows only `<style>` elements carrying this load's nonce
// (bmacd puts a new one in index.html each time). xterm and Radix create
// theirs at run time without one, so every `<style>` made here gets it. Must
// be imported before anything that creates elements.

const meta = document.querySelector<HTMLMetaElement>('meta[name="bmac-style-nonce"]');
const nonce = meta?.content;
meta?.remove();

if (nonce && !nonce.startsWith("__")) {
  const create = Document.prototype.createElement;
  Document.prototype.createElement = function (this: Document, tagName: string, options?: ElementCreationOptions) {
    const element = create.call(this, tagName, options);
    if (tagName.toLowerCase() === "style") element.setAttribute("nonce", nonce);
    return element;
  } as typeof Document.prototype.createElement;
}

export {};
