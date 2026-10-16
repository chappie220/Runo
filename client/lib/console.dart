import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:xterm/xterm.dart' as xterm;

import 'text.dart';

/// [terminal] on screen, with the keys of Windows Terminal: copy and paste,
/// text size, scrolling the history.
class Console extends StatefulWidget {
  const Console({
    super.key,
    required this.terminal,
    required this.focusNode,
    this.readOnly = false,
  });

  final xterm.Terminal terminal;
  final FocusNode focusNode;
  final bool readOnly;

  @override
  State<Console> createState() => ConsoleState();
}

class ConsoleState extends State<Console> {
  final t = T.of;
  final controller = xterm.TerminalController();
  final scroll = ScrollController();
  double fontSize = _fontSize;

  static const _fontSize = 14.0;

  bool get selected => controller.selection != null;

  Future<void> copy() async {
    final selection = controller.selection;
    if (selection == null) return;
    final text = widget.terminal.buffer.getText(selection);
    controller.clearSelection();
    await Clipboard.setData(ClipboardData(text: text));
  }

  Future<void> paste() async {
    final text = (await Clipboard.getData(Clipboard.kTextPlain))?.text;
    if (text == null || text.isEmpty) return;
    controller.clearSelection();
    widget.terminal.paste(text);
  }

  void selectAll() => controller.setSelection(
    widget.terminal.buffer.createAnchor(0, 0),
    widget.terminal.buffer.createAnchor(
      widget.terminal.viewWidth,
      widget.terminal.buffer.height - 1,
    ),
    mode: xterm.SelectionMode.line,
  );

  void zoom(double? by) => setState(
    () => fontSize = by == null ? _fontSize : (fontSize + by).clamp(8, 32),
  );

  /// Scrolls the history to [to], given the line height and the page
  /// height.
  void scrollTo(double Function(double at, double line, double page) to) {
    if (!scroll.hasClients) return;
    final position = scroll.position;
    final page = position.viewportDimension;
    position.jumpTo(
      to(
        position.pixels,
        page / widget.terminal.viewHeight,
        page,
      ).clamp(position.minScrollExtent, position.maxScrollExtent),
    );
  }

  /// The keys of Windows Terminal, and what each does here.
  List<_Binding> get _bindings {
    SingleActivator ctrl(LogicalKeyboardKey key, {bool shift = false}) =>
        SingleActivator(key, control: true, shift: shift);
    SingleActivator ctrlShift(LogicalKeyboardKey key) => ctrl(key, shift: true);
    return [
      _Binding(
        'Ctrl+C',
        t.keyCopyOr,
        [ctrl(LogicalKeyboardKey.keyC)],
        copy,
        when: () => selected,
      ),
      _Binding('Ctrl+Shift+C · Ctrl+Insert', t.keyCopy, [
        ctrlShift(LogicalKeyboardKey.keyC),
        ctrl(LogicalKeyboardKey.insert),
      ], copy),
      _Binding('Ctrl+V · Ctrl+Shift+V · Shift+Insert', t.keyPaste, [
        ctrl(LogicalKeyboardKey.keyV),
        ctrlShift(LogicalKeyboardKey.keyV),
        const SingleActivator(LogicalKeyboardKey.insert, shift: true),
      ], paste),
      _Binding('Ctrl+Shift+A', t.keySelectAll, [
        ctrlShift(LogicalKeyboardKey.keyA),
      ], selectAll),
      _Binding('Ctrl+=', t.keyBigger, [
        ctrl(LogicalKeyboardKey.equal),
        ctrlShift(LogicalKeyboardKey.equal),
        ctrl(LogicalKeyboardKey.numpadAdd),
      ], () => zoom(1)),
      _Binding('Ctrl+-', t.keySmaller, [
        ctrl(LogicalKeyboardKey.minus),
        ctrl(LogicalKeyboardKey.numpadSubtract),
      ], () => zoom(-1)),
      _Binding('Ctrl+0', t.keyResetSize, [
        ctrl(LogicalKeyboardKey.digit0),
        ctrl(LogicalKeyboardKey.numpad0),
      ], () => zoom(null)),
      _Binding(
        'Ctrl+Shift+↑ / ↓',
        t.keyLine,
        [ctrlShift(LogicalKeyboardKey.arrowUp)],
        () {
          scrollTo((at, line, _) => at - line);
        },
        more: [
          _Shortcut(
            ctrlShift(LogicalKeyboardKey.arrowDown),
            () => scrollTo((at, line, _) => at + line),
          ),
        ],
      ),
      _Binding(
        'Ctrl+Shift+PgUp / PgDn',
        t.keyPage,
        [ctrlShift(LogicalKeyboardKey.pageUp)],
        () {
          scrollTo((at, _, page) => at - page);
        },
        more: [
          _Shortcut(
            ctrlShift(LogicalKeyboardKey.pageDown),
            () => scrollTo((at, _, page) => at + page),
          ),
        ],
      ),
      _Binding(
        'Ctrl+Shift+Home / End',
        t.keyEnds,
        [ctrlShift(LogicalKeyboardKey.home)],
        () {
          scrollTo((_, _, _) => double.negativeInfinity);
        },
        more: [
          _Shortcut(
            ctrlShift(LogicalKeyboardKey.end),
            () => scrollTo((_, _, _) => double.infinity),
          ),
        ],
      ),
    ];
  }

  /// Lists the shortcuts.
  void help() {
    final theme = Theme.of(context);
    showDialog<void>(
      context: context,
      builder: (context) => AlertDialog(
        icon: const Icon(Icons.keyboard_outlined),
        title: Text(t.shortcuts),
        content: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              for (final (keys, what) in [
                for (final b in _bindings) (b.label, b.what),
                (null, t.keyClick),
              ])
                Padding(
                  padding: const EdgeInsets.symmetric(vertical: 6),
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(what),
                      if (keys != null)
                        Text(
                          keys,
                          style: theme.textTheme.bodySmall?.copyWith(
                            color: theme.colorScheme.primary,
                            fontWeight: FontWeight.w600,
                          ),
                        ),
                    ],
                  ),
                ),
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: Text(MaterialLocalizations.of(context).closeButtonLabel),
          ),
        ],
      ),
    );
  }

  @override
  void dispose() {
    controller.dispose();
    scroll.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final bindings = _bindings;
    return Actions(
      actions: {_Run: _RunAction()},
      child: xterm.TerminalView(
        widget.terminal,
        controller: controller,
        scrollController: scroll,
        focusNode: widget.focusNode,
        autofocus: true,
        readOnly: widget.readOnly,
        padding: const EdgeInsets.all(4),
        textStyle: xterm.TerminalStyle(fontSize: fontSize),
        shortcuts: {
          for (final binding in bindings)
            for (final shortcut in binding.shortcuts)
              shortcut.keys: _Run(shortcut.run, binding.when),
        },
        onSecondaryTapUp: (_, _) => selected ? copy() : paste(),
      ),
    );
  }
}

/// Keys that do one thing in the terminal, as [label] names them.
class _Binding {
  _Binding(
    this.label,
    this.what,
    List<SingleActivator> keys,
    VoidCallback run, {
    this.when,
    List<_Shortcut> more = const [],
  }) : shortcuts = [for (final key in keys) _Shortcut(key, run), ...more];

  final String label;
  final String what;
  final List<_Shortcut> shortcuts;

  /// Whether the keys do it now; when not, they go to the session.
  final bool Function()? when;
}

class _Shortcut {
  _Shortcut(this.keys, this.run);

  final SingleActivator keys;
  final VoidCallback run;
}

class _Run extends Intent {
  const _Run(this.run, this.when);

  final VoidCallback run;
  final bool Function()? when;
}

class _RunAction extends Action<_Run> {
  @override
  bool isEnabled(_Run intent) => intent.when?.call() ?? true;

  @override
  void invoke(_Run intent) => intent.run();
}
