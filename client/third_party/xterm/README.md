# xterm 4.0.0, patched

The library of [xterm.dart](https://github.com/TerminalStudio/xterm.dart) 4.0.0 (MIT, see LICENSE), with two changes, each marked `Runo:` in the code:

- `lib/src/ui/custom_text_edit.dart`: on Linux the engine reports a finished input-method composition up to three times before the terminal's reset reaches it, and 4.0.0 sent the word once per report. New text is now measured from what was already handled, and the reset waits until the reports stop. `test/terminal_input_test.dart` covers it.
- `lib/src/core/buffer/buffer.dart`: characters that belong to the one before them take the cells unicode-width (and so ratatui) gives them: zero-width characters take no cell, a skin tone or an emoji joined by a zero-width joiner stays inside the emoji it follows, and an emoji variation selector makes a narrow symbol such as ⚠ wide. The cell keeps the first character only. `test/tui_render_test.dart` covers it.

Back to the published package once upstream handles these.
