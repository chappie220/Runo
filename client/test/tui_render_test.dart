import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:xterm/xterm.dart';

/// A row as the terminal shows it: one character per cell, a wide
/// character's second cell left out.
String shown(BufferLine line, int width) {
  final row = StringBuffer();
  for (var x = 0; x < width; x++) {
    final c = line.getCodePoint(x);
    if (c != 0) {
      row.writeCharCode(c);
    } else if (x == 0 || line.getWidth(x - 1) != 2) {
      row.write(' ');
    }
  }
  return row.toString().trimRight();
}

/// What ratatui drew, with an emoji's skin tone, joined emoji and
/// variation selector folded into its first character, as the terminal
/// keeps it.
String folded(String row) => row
    .replaceAll(RegExp('‍.', unicode: true), '')
    .replaceAll(RegExp('[\u{1F3FB}-\u{1F3FF}️]', unicode: true), '');

void main() {
  // `runo tui` output (src/tui/backend.rs) for a chat in Chinese and
  // English with emoji, frame by frame as a reply streams in, with the
  // screen ratatui meant each frame to leave.
  test('runo tui frames end up on screen as ratatui drew them', () {
    final file = File('test/fixtures/tui_frames.json.gz');
    final dump = jsonDecode(
      utf8.decode(gzip.decode(file.readAsBytesSync())),
    ) as Map<String, dynamic>;
    final width = dump['width'] as int;
    final height = dump['height'] as int;
    final terminal = Terminal()..resize(width, height);
    final frames = dump['frames'] as List;
    for (final (n, frame) in frames.indexed) {
      terminal.write(frame['bytes'] as String);
      final screen = (frame['screen'] as List).cast<String>();
      for (var y = 0; y < height; y++) {
        final line = terminal.buffer.lines[terminal.buffer.scrollBack + y];
        expect(
          shown(line, width),
          folded(screen[y]),
          reason: 'frame $n, row $y',
        );
      }
    }
  });

  test('emoji sequences take the cells unicode-width gives them', () {
    final terminal = Terminal()..resize(20, 1);
    terminal.write('⚠️|👍🏻|🧑‍💻|é|好');
    final line = terminal.buffer.lines[0];
    expect(shown(line, 20), '⚠|👍|🧑|e|好');
    // ⚠️ 2, 👍🏻 2, 🧑‍💻 2, é 1, 好 2, and four bars.
    expect(terminal.buffer.cursorX, 13);
  });
}
