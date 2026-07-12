import { StreamLanguage } from "@codemirror/language";
import type { Extension } from "@codemirror/state";

const keywords = new Set([
  "use",
  "fn",
  "let",
  "for",
  "in",
  "if",
  "else",
  "match",
  "return",
  "true",
  "false",
]);

/** Lightweight highlighting for vmlab's WScript files. */
export const wscriptLanguage: Extension = StreamLanguage.define({
  token(stream) {
    if (stream.eatSpace()) return null;
    if (stream.match("//")) {
      stream.skipToEnd();
      return "comment";
    }
    if (stream.peek() === '"') {
      stream.next();
      let escaped = false;
      while (!stream.eol()) {
        const char = stream.next();
        if (char === '"' && !escaped) break;
        escaped = char === "\\" && !escaped;
      }
      return "string";
    }
    if (stream.match(/^-?\d+(?:\.\d+)?/)) return "number";
    if (stream.match(/^[A-Za-z_][\w:]*/)) {
      return keywords.has(stream.current()) ? "keyword" : "variableName";
    }
    stream.next();
    return null;
  },
});
