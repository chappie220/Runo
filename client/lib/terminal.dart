import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:xterm/xterm.dart' as xterm;

import 'src/rust/api/client.dart' as rust;
import 'store.dart';
import 'text.dart';

/// A terminal on the device: the Agent, a shell, or a session shown again.
/// Leaving it leaves the session running there.
class TerminalPage extends StatefulWidget {
  const TerminalPage({
    super.key,
    required this.connection,
    required this.kind,
    required this.title,
    this.attach,
  });

  final rust.Connection connection;

  /// "agent" or "shell".
  final String kind;
  final String title;
  final BigInt? attach;

  @override
  State<TerminalPage> createState() => _TerminalPageState();
}

class _TerminalPageState extends State<TerminalPage> {
  final t = T.of;
  final terminal = xterm.Terminal(maxLines: 10000);
  final focus = FocusNode();
  late final Sink<List<int>> screen = const Utf8Decoder(allowMalformed: true)
      .startChunkedConversion(_Writer(terminal.write));
  rust.Terminal? remote;
  StreamSubscription<rust.TerminalEvent>? output;
  bool ended = false;
  bool ctrl = false;

  @override
  void initState() {
    super.initState();
    // Opens once the view has laid out, so the session starts at its size.
    WidgetsBinding.instance.addPostFrameCallback((_) => open());
  }

  Future<void> open() async {
    try {
      final remote = await widget.connection.terminal(
        kind: widget.kind,
        attach: widget.attach,
        cols: terminal.viewWidth,
        rows: terminal.viewHeight,
      );
      if (!mounted) {
        await remote.detach();
        return;
      }
      this.remote = remote;
      terminal.onOutput = type;
      terminal.onResize = (cols, rows, _, _) {
        if (!ended) remote.resize(cols: cols, rows: rows).ignore();
      };
      output = remote.output().listen(show, onDone: () => end(null, null));
      setState(() {});
      focus.requestFocus();
    } catch (error) {
      end(null, message(error));
    }
  }

  void type(String keys) {
    final remote = this.remote;
    if (remote == null || ended) return;
    if (ctrl && keys.length == 1) {
      final c = keys.toUpperCase().codeUnitAt(0);
      if (c >= 0x40 && c < 0x60) keys = String.fromCharCode(c - 0x40);
      setState(() => ctrl = false);
    }
    remote.write(data: utf8.encode(keys));
  }

  void show(rust.TerminalEvent event) {
    if (event.data.isNotEmpty) screen.add(event.data);
    if (event.ended || event.error != null) end(event.code, event.error);
  }

  void end(int? code, String? error) {
    if (ended || !mounted) return;
    terminal.write('\r\n\x1b[2m${error ?? t.sessionEnded(code)}\x1b[0m\r\n');
    setState(() => ended = true);
  }

  @override
  void dispose() {
    output?.cancel();
    if (!ended) remote?.detach().ignore();
    focus.dispose();
    super.dispose();
  }

  Widget key(String label, String keys) => TextButton(
    onPressed: ended ? null : () => type(keys),
    child: Text(label),
  );

  @override
  Widget build(BuildContext context) {
    final touch = Platform.isAndroid;
    return Scaffold(
      appBar: AppBar(
        title: Text(widget.title),
        actions: [
          if (!ended)
            IconButton(
              tooltip: t.detach,
              icon: const Icon(Icons.logout),
              onPressed: () => Navigator.pop(context),
            ),
        ],
      ),
      body: SafeArea(
        child: Column(
          children: [
            Expanded(
              child: xterm.TerminalView(
                terminal,
                focusNode: focus,
                autofocus: true,
                readOnly: ended,
                padding: const EdgeInsets.all(4),
                textStyle: const xterm.TerminalStyle(fontSize: 14),
              ),
            ),
            if (touch)
              SingleChildScrollView(
                scrollDirection: Axis.horizontal,
                child: Row(
                  children: [
                    key('Esc', '\x1b'),
                    key('Tab', '\t'),
                    TextButton(
                      onPressed: ended
                          ? null
                          : () => setState(() => ctrl = !ctrl),
                      style: ctrl
                          ? TextButton.styleFrom(
                              backgroundColor: Theme.of(context)
                                  .colorScheme
                                  .primaryContainer,
                            )
                          : null,
                      child: const Text('Ctrl'),
                    ),
                    key('↑', '\x1b[A'),
                    key('↓', '\x1b[B'),
                    key('←', '\x1b[D'),
                    key('→', '\x1b[C'),
                    key('^C', '\x03'),
                    key('|', '|'),
                    key('/', '/'),
                    key('-', '-'),
                  ],
                ),
              ),
          ],
        ),
      ),
    );
  }
}

class _Writer implements Sink<String> {
  _Writer(this.write);

  final void Function(String) write;

  @override
  void add(String data) => write(data);

  @override
  void close() {}
}
