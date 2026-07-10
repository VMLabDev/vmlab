// A small, best-effort CodeMirror highlighter for WCL (HCL-like): comments,
// strings, import paths (<vmlab.wcl>), numbers, booleans, identifiers. The
// returned token names map to @lezer/highlight tags via CodeMirror's default
// table. Passed to @forge/code's CodeEditor via its `language` prop.

import { StreamLanguage } from "@codemirror/language";
import type { Extension } from "@codemirror/state";

export const wclLanguage: Extension = StreamLanguage.define({
  token(stream) {
    if (stream.eatSpace()) return null;
    if (stream.match("//") || stream.match("#")) {
      stream.skipToEnd();
      return "comment";
    }
    if (stream.match("/*")) {
      while (!stream.eol() && !stream.match("*/")) stream.next();
      return "comment";
    }
    const ch = stream.peek();
    if (ch === '"') {
      stream.next();
      let escaped = false;
      while (!stream.eol()) {
        const c = stream.next();
        if (c === '"' && !escaped) break;
        escaped = c === "\\" && !escaped;
      }
      return "string";
    }
    if (ch === "<" && stream.match(/^<[^>]*>/)) return "string";
    if (stream.match(/^-?\d+(\.\d+)?/)) return "number";
    if (stream.match(/^(true|false|null)\b/)) return "atom";
    if (stream.match(/^[A-Za-z_][\w.-]*/)) return "variableName";
    stream.next();
    return null;
  },
});
