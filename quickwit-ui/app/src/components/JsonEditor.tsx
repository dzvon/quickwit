// Copyright (C) 2021 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

import Editor from "@monaco-editor/react";
import { useCallback } from "react";
import { QUICKWIT_BLUE, QUICKWIT_LIGHT_GREY } from "../utils/theme";

export function JsonEditor({content, resizeOnMount}: {content: any, resizeOnMount: boolean}) {
  // setting editor height based on lines height and count to stretch and fit its content
  const onMount = useCallback((editor) => {
    if (!resizeOnMount) {
      return;
    } 
    const editorElement = editor.getDomNode();

    if (!editorElement) {
      return;
    }

    // TODO: use enum for the lineHeight option index.
    const lineHeight = editor.getOption(58);
    const lineCount = editor.getModel()?.getLineCount() || 1;
    const height = editor.getTopForLineNumber(lineCount + 1) + 2 * lineHeight;

    editorElement.style.height = `${height}px`;
    editor.layout();
  }, []);

  function beforeMount(monaco: any) {
    monaco.editor.defineTheme('quickwit-light', {
      base: 'vs',
      inherit: true,
      rules: [
        { token: 'comment', foreground: '#1F232A', fontStyle: 'italic' },
        { token: 'keyword', foreground: QUICKWIT_BLUE }
      ],
      colors: {
        'editor.comment.foreground': '#CBD1DE',
        'editor.foreground': '#000000',
        'editor.background': QUICKWIT_LIGHT_GREY,
        'editorLineNumber.foreground': 'black',
        'editor.lineHighlightBackground': '#DFE0E1',
      },
    });
  }

  return (
    <Editor
      language='json'
      value={JSON.stringify(content, null, 2)}
      beforeMount={beforeMount}
      onMount={onMount}
      options={{
        readOnly: true,
        fontFamily: 'monospace',
        minimap: {
          enabled: false,
        },
        renderLineHighlight: "gutter",
        fontSize: 12,
        fixedOverflowWidgets: true,
        scrollBeyondLastLine: false,
        automaticLayout: true,
      }}
      theme='quickwit-light'
    />
  )
}