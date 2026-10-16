import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:xterm/xterm.dart';

void main() {
  // On Linux the engine reports a finished composition up to three times
  // (commit, preedit cleared, preedit ended) before the terminal's reset
  // reaches it; the text goes out once.
  testWidgets('sends text from an input method once', (tester) async {
    final terminal = Terminal();
    final sent = <String>[];
    terminal.onOutput = sent.add;
    await tester.pumpWidget(
      MaterialApp(home: TerminalView(terminal, autofocus: true)),
    );
    await tester.pump();

    const composing = TextEditingValue(
      text: 'nihao',
      selection: TextSelection.collapsed(offset: 5),
      composing: TextRange(start: 0, end: 5),
    );
    const done = TextEditingValue(
      text: '你好',
      selection: TextSelection.collapsed(offset: 2),
    );
    tester.testTextInput.updateEditingValue(composing);
    for (var i = 0; i < 3; i++) {
      tester.testTextInput.updateEditingValue(done);
    }
    await tester.pump(const Duration(milliseconds: 200));
    expect(sent.join(), '你好');

    // The same word again, once the terminal has its field back.
    tester.testTextInput.updateEditingValue(composing);
    tester.testTextInput.updateEditingValue(done);
    await tester.pump(const Duration(milliseconds: 200));
    expect(sent.join(), '你好你好');
  });
}
