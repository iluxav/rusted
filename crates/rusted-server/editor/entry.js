// Monaco with the TypeScript language service — the console editor's engine.
// Bundled by `make editor-js`; workers are bundled separately (see Makefile).
// The basic-languages contributions register the file extensions and
// tokenizers; without them models fall back to plaintext and the language
// service never engages.
import * as monaco from "monaco-editor/esm/vs/editor/editor.api";
import "monaco-editor/esm/vs/basic-languages/typescript/typescript.contribution";
import "monaco-editor/esm/vs/basic-languages/javascript/javascript.contribution";
import "monaco-editor/esm/vs/language/typescript/monaco.contribution";
import "monaco-editor/esm/vs/editor/contrib/format/browser/formatActions";

window.monaco = monaco;
