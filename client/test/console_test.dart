import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:runo_app/console.dart';
import 'package:xterm/xterm.dart';

void main() {
  late Terminal terminal;
  late List<String> sent;
  late String? clipboard;
  final console = GlobalKey<ConsoleState>();

  Future<void> start(WidgetTester tester) async {
    terminal = Terminal();
    sent = [];
    clipboard = null;
    terminal.onOutput = sent.add;
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          clipboard = (call.arguments as Map)['text'] as String?;
        }
        if (call.method == 'Clipboard.getData') return {'text': clipboard};
        return null;
      },
    );
    await tester.pumpWidget(
      MaterialApp(
        home: Console(key: console, terminal: terminal, focusNode: FocusNode()),
      ),
    );
    await tester.pump();
  }

  Future<void> press(
    WidgetTester tester,
    LogicalKeyboardKey key, {
    bool control = false,
    bool shift = false,
  }) async {
    if (control) await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    if (shift) await tester.sendKeyDownEvent(LogicalKeyboardKey.shiftLeft);
    await tester.sendKeyEvent(key);
    if (shift) await tester.sendKeyUpEvent(LogicalKeyboardKey.shiftLeft);
    if (control) await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    await tester.pump();
  }

  testWidgets('Shift+Insert pastes', (tester) async {
    await start(tester);
    clipboard = 'ls -la';
    await press(tester, LogicalKeyboardKey.insert, shift: true);
    expect(sent.join(), 'ls -la');
  });

  testWidgets('Ctrl+C interrupts with nothing selected', (tester) async {
    await start(tester);
    await press(tester, LogicalKeyboardKey.keyC, control: true);
    expect(sent.join(), '\x03');
  });

  testWidgets('Ctrl+C copies a selection instead', (tester) async {
    await start(tester);
    terminal.write('hello');
    await press(tester, LogicalKeyboardKey.keyA, control: true, shift: true);
    await press(tester, LogicalKeyboardKey.keyC, control: true);
    expect(clipboard?.trim(), 'hello');
    expect(sent, isEmpty);
    expect(console.currentState!.selected, isFalse);
  });

  testWidgets('Ctrl+A goes to the session', (tester) async {
    await start(tester);
    await press(tester, LogicalKeyboardKey.keyA, control: true);
    expect(sent.join(), '\x01');
  });

  testWidgets('Ctrl+= and Ctrl+0 size the text', (tester) async {
    await start(tester);
    await press(tester, LogicalKeyboardKey.equal, control: true);
    expect(console.currentState!.fontSize, 15);
    await press(tester, LogicalKeyboardKey.digit0, control: true);
    expect(console.currentState!.fontSize, 14);
  });
}
