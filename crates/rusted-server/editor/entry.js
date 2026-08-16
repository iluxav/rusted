// The console editor's building blocks, exposed as one global for the
// editor.html template to assemble. Bundled by `make editor-js`.
import { EditorView, basicSetup } from "codemirror";
import { keymap } from "@codemirror/view";
import { indentWithTab } from "@codemirror/commands";
import { javascript } from "@codemirror/lang-javascript";
import { oneDark } from "@codemirror/theme-one-dark";

window.RustedEditor = { EditorView, basicSetup, keymap, indentWithTab, javascript, oneDark };
