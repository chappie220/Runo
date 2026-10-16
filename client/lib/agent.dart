import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'changes.dart';
import 'feed.dart';
import 'main.dart';
import 'markdown.dart';
import 'src/rust/api/client.dart' as rust;
import 'store.dart';
import 'text.dart';
import 'theme.dart';
import 'widgets.dart';

/// The Agent drawn from its event stream (4.11.5): messages, cards, the
/// device's status and a real text box. Leaving leaves it running on the
/// device; a lost connection is picked up again from the last event seen.
class AgentPage extends StatefulWidget {
  const AgentPage({
    super.key,
    required this.open,
    required this.title,
    this.connection,
  });

  final OpenFeed open;
  final String title;

  /// For the banner about changes waiting for confirmation.
  final rust.Connection? connection;

  @override
  State<AgentPage> createState() => AgentPageState();
}

class AgentPageState extends State<AgentPage> {
  final t = T.of;
  final log = FeedLog();
  final input = TextEditingController();
  late final focus = FocusNode(onKeyEvent: key);
  FeedLink? link;
  StreamSubscription<rust.FeedFrame>? events;
  bool joining = true;
  bool ended = false;

  /// Why the connection went, until it is back.
  String? lost;
  int tries = 0;
  Timer? retry;

  @override
  void initState() {
    super.initState();
    open();
  }

  @override
  void dispose() {
    retry?.cancel();
    events?.cancel();
    link?.detach().ignore();
    focus.dispose();
    input.dispose();
    log.dispose();
    super.dispose();
  }

  /// Opens the feed, from after the last event seen.
  Future<void> open() async {
    retry?.cancel();
    setState(() => joining = true);
    try {
      final link = await widget.open(log.seq);
      if (!mounted) {
        link.detach().ignore();
        return;
      }
      setState(() {
        this.link = link;
        joining = false;
        lost = null;
        tries = 0;
      });
      events = link.events().listen(
        frame,
        onDone: () {
          if (this.link == link) lose('');
        },
      );
      focus.requestFocus();
    } catch (error) {
      lose(message(error));
    }
  }

  void frame(rust.FeedFrame frame) {
    final event = frame.event;
    if (event != null) {
      try {
        log.add(event);
      } on FormatException {
        // Not an event: nothing to show.
      }
    }
    if (!frame.ended) return;
    events?.cancel();
    if (frame.error == null) {
      setState(() {
        link = null;
        ended = true;
      });
    } else {
      lose(frame.error!);
    }
  }

  /// The connection went: tries again a few times, then waits for the
  /// person.
  void lose(String why) {
    if (!mounted) return;
    events?.cancel();
    setState(() {
      link = null;
      joining = false;
      lost = why;
    });
    if (tries < 3) {
      tries++;
      retry = Timer(Duration(seconds: 1 << tries), open);
    }
  }

  Future<void> run(Future<void> Function(FeedLink) call) async {
    final link = this.link;
    if (link == null) return;
    try {
      await call(link);
    } catch (error) {
      if (mounted) complain(context, error);
    }
  }

  /// Sends what is in the box, exactly as it is.
  Future<void> send() async {
    final text = input.text;
    final link = this.link;
    if (text.trim().isEmpty || link == null) return;
    input.clear();
    try {
      await link.send(text);
    } catch (error) {
      if (!mounted) return;
      if (input.text.isEmpty) input.text = text;
      complain(context, error);
    }
  }

  /// Enter sends and Shift+Enter starts a new line; while an input method
  /// is composing, Enter is its own.
  KeyEventResult key(FocusNode node, KeyEvent event) {
    if (event is KeyUpEvent) return KeyEventResult.ignored;
    if (event.logicalKey != LogicalKeyboardKey.enter &&
        event.logicalKey != LogicalKeyboardKey.numpadEnter) {
      return KeyEventResult.ignored;
    }
    final value = input.value;
    if (value.composing.isValid && !value.composing.isCollapsed) {
      return KeyEventResult.ignored;
    }
    if (HardwareKeyboard.instance.isShiftPressed) {
      final at = value.selection.isValid
          ? value.selection
          : TextSelection.collapsed(offset: value.text.length);
      input.value = TextEditingValue(
        text: value.text.replaceRange(at.start, at.end, '\n'),
        selection: TextSelection.collapsed(offset: at.start + 1),
      );
      return KeyEventResult.handled;
    }
    if (event is KeyDownEvent) send();
    return KeyEventResult.handled;
  }

  Widget item(Object item) => switch (item) {
    FeedMessage message => MessageView(message),
    FeedCard card => FeedCardView(
      key: ValueKey(card.id),
      card: card,
      answer: (action, value) =>
          run((link) => link.answer(card.id, action, value)),
      cancel: () => run((link) => link.cancel(card.id)),
    ),
    _ => const SizedBox.shrink(),
  };

  Widget banner(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    final waiting = retry?.isActive ?? false;
    return Material(
      color: colors.errorContainer,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(16, 6, 8, 6),
        child: Row(
          children: [
            Icon(Icons.cloud_off, color: colors.onErrorContainer, size: 20),
            const SizedBox(width: 12),
            Expanded(
              child: Text(
                joining || waiting
                    ? t.rejoining
                    : lost!.isEmpty
                    ? t.lost
                    : '${t.lost}: $lost',
                style: TextStyle(color: colors.onErrorContainer),
              ),
            ),
            TextButton(
              onPressed: joining ? null : open,
              child: Text(t.reconnect),
            ),
          ],
        ),
      ),
    );
  }

  Widget box(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    final live = link != null;
    return Material(
      color: colors.surfaceContainer,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 8, 8, 8),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.end,
          children: [
            Expanded(
              child: TextField(
                key: const Key('feed-input'),
                controller: input,
                focusNode: focus,
                enabled: live,
                minLines: 1,
                maxLines: 8,
                keyboardType: TextInputType.multiline,
                textInputAction: TextInputAction.newline,
                decoration: InputDecoration(
                  hintText: ended ? t.sessionEnded(null) : t.sayHint,
                  isDense: true,
                ),
              ),
            ),
            const SizedBox(width: 8),
            if (log.busy)
              IconButton.filledTonal(
                key: const Key('feed-stop'),
                tooltip: t.stop,
                icon: const Icon(Icons.stop),
                onPressed: live ? () => run((link) => link.cancel(null)) : null,
              ),
            IconButton.filled(
              key: const Key('feed-send'),
              tooltip: t.send,
              icon: const Icon(Icons.arrow_upward),
              onPressed: live ? send : null,
            ),
          ],
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final page = ListenableBuilder(
      listenable: log,
      builder: (context, _) {
        final items = log.items;
        final status = log.status;
        return Column(
          children: [
            if (status != null) StatusStrip(status),
            if (lost != null) banner(context),
            Expanded(
              child: joining && items.isEmpty
                  ? const Center(child: CircularProgressIndicator())
                  : items.isEmpty
                  ? Content(
                      child: Padding(
                        padding: const EdgeInsets.all(16),
                        child: Empty(
                          icon: Icons.smart_toy_outlined,
                          text: t.feedEmpty,
                        ),
                      ),
                    )
                  : LayoutBuilder(
                      builder: (context, size) {
                        final side = size.maxWidth > 992
                            ? (size.maxWidth - 960) / 2
                            : 16.0;
                        // Upside down, so the newest stays in view while
                        // a reply grows.
                        return ListView.builder(
                          reverse: true,
                          padding: EdgeInsets.fromLTRB(side, 12, side, 12),
                          itemCount: items.length,
                          itemBuilder: (context, i) => Padding(
                            padding: const EdgeInsets.symmetric(vertical: 6),
                            child: item(items[items.length - 1 - i]),
                          ),
                        );
                      },
                    ),
            ),
            if (log.busy) const LinearProgressIndicator(minHeight: 2),
            box(context),
          ],
        );
      },
    );
    final connection = widget.connection;
    return Scaffold(
      appBar: AppBar(
        title: Row(
          children: [
            const Icon(Icons.smart_toy_outlined, size: 22),
            const SizedBox(width: 10),
            Flexible(child: Text(widget.title)),
          ],
        ),
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
        child: connection == null
            ? page
            : ChangeBanner(connection: connection, child: page),
      ),
    );
  }
}

/// One message: the person's on the right, the Agent's as Markdown, a
/// note from Runo small and grey.
class MessageView extends StatelessWidget {
  const MessageView(this.message, {super.key});

  final FeedMessage message;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    switch (message.role) {
      case 'you':
        return Align(
          alignment: Alignment.centerRight,
          child: FractionallySizedBox(
            widthFactor: .85,
            alignment: Alignment.centerRight,
            child: Align(
              alignment: Alignment.centerRight,
              child: Container(
                padding: const EdgeInsets.symmetric(
                  horizontal: 14,
                  vertical: 10,
                ),
                decoration: BoxDecoration(
                  color: colors.primaryContainer,
                  borderRadius: BorderRadius.circular(18),
                ),
                child: SelectableText(
                  message.text,
                  style: theme.textTheme.bodyLarge?.copyWith(
                    color: colors.onPrimaryContainer,
                  ),
                ),
              ),
            ),
          ),
        );
      case 'note':
        return Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(Icons.info_outline, size: 18, color: colors.outline),
            const SizedBox(width: 8),
            Expanded(
              child: SelectableText(
                message.text,
                style: theme.textTheme.bodyMedium?.copyWith(
                  color: colors.onSurfaceVariant,
                ),
              ),
            ),
          ],
        );
      default:
        return Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Tile(Icons.smart_toy_outlined, size: 32),
            const SizedBox(width: 12),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  SelectionArea(child: Markdown(message.text)),
                  if (!message.done)
                    const Padding(
                      padding: EdgeInsets.only(top: 6),
                      child: Align(
                        alignment: Alignment.centerLeft,
                        child: SizedBox(
                          width: 14,
                          height: 14,
                          child: CircularProgressIndicator(strokeWidth: 2),
                        ),
                      ),
                    ),
                ],
              ),
            ),
          ],
        );
    }
  }
}

(String, Color) cardState(BuildContext context, String state) {
  final t = T.of;
  final colors = Theme.of(context).colorScheme;
  return switch (state) {
    'waiting' => (t.cardWaiting, colors.tertiary),
    'running' => (t.cardRunning, colors.primary),
    'done' => (t.cardDone, good(context)),
    'failed' => (t.cardFailed, colors.error),
    _ => (t.cardCancelled, colors.outline),
  };
}

Widget stateIcon(BuildContext context, String state, {double size = 18}) {
  final colors = Theme.of(context).colorScheme;
  return switch (state) {
    'running' => SizedBox(
      width: size,
      height: size,
      child: const Padding(
        padding: EdgeInsets.all(2),
        child: CircularProgressIndicator(strokeWidth: 2),
      ),
    ),
    'done' => Icon(Icons.check_circle, size: size, color: good(context)),
    'failed' => Icon(Icons.error, size: size, color: colors.error),
    'waiting' => Icon(Icons.pending, size: size, color: colors.tertiary),
    _ => Icon(Icons.do_not_disturb_on, size: size, color: colors.outline),
  };
}

/// A card from the Agent: a task, a confirmation, a secret, a choice, a
/// file or the first start's checklist.
class FeedCardView extends StatefulWidget {
  const FeedCardView({
    super.key,
    required this.card,
    required this.answer,
    required this.cancel,
  });

  final FeedCard card;
  final Future<void> Function(String action, String? value) answer;

  /// Stops the task.
  final Future<void> Function() cancel;

  @override
  State<FeedCardView> createState() => _FeedCardViewState();
}

class _FeedCardViewState extends State<FeedCardView> {
  final t = T.of;
  final secret = TextEditingController();
  final chosen = <String>{};
  final open = <int>{};
  bool busy = false;

  FeedCard get card => widget.card;
  Map<String, dynamic> get body => card.body;

  @override
  void dispose() {
    secret.dispose();
    super.dispose();
  }

  Future<void> press(FeedAction action, [String? value]) async {
    setState(() => busy = true);
    try {
      await widget.answer(action.id, value);
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  /// What the card says it can do, or for a question still open without
  /// buttons of its own, the one that answers it.
  List<FeedAction> get actions {
    if (card.actions.isNotEmpty || !card.waiting) return card.actions;
    return switch (card.type) {
      'secret' when body['saved'] != true => [FeedAction.of('save', t.save)],
      'choice' => [FeedAction.of('choose', t.choose)],
      _ => const [],
    };
  }

  /// The value an action carries: the secret for its answering button,
  /// the options picked for a choice.
  String? valueFor(FeedAction action) => switch (card.type) {
    'secret' when action.style == 'primary' => secret.text,
    'choice' when action.style != 'danger' => chosen.join(','),
    _ => null,
  };

  bool allowed(FeedAction action) {
    if (busy) return false;
    if (card.type == 'secret' && action.style == 'primary') {
      final min = (body['min_len'] as num?)?.toInt() ?? 0;
      return secret.text.isNotEmpty && secret.text.length >= min;
    }
    if (card.type == 'choice' && action.style != 'danger') {
      return chosen.isNotEmpty;
    }
    return true;
  }

  void onAction(FeedAction action) {
    final value = valueFor(action);
    // A secret leaves no copy here once it is on its way.
    if (card.type == 'secret') secret.clear();
    press(action, value);
  }

  Widget button(FeedAction action) {
    final onPressed = allowed(action) ? () => onAction(action) : null;
    final label = Text(action.label);
    final key = Key('action-${card.id}-${action.id}');
    return switch (action.style) {
      'primary' => FilledButton(key: key, onPressed: onPressed, child: label),
      'danger' => FilledButton(
        key: key,
        style: FilledButton.styleFrom(
          backgroundColor: Theme.of(context).colorScheme.error,
          foregroundColor: Theme.of(context).colorScheme.onError,
        ),
        onPressed: onPressed,
        child: label,
      ),
      _ => OutlinedButton(key: key, onPressed: onPressed, child: label),
    };
  }

  IconData get icon => switch (card.type) {
    'task' => Icons.construction,
    'confirm' => Icons.gpp_maybe_outlined,
    'secret' => Icons.key,
    'choice' => Icons.checklist_rtl,
    'file' => Icons.insert_drive_file_outlined,
    'checklist' => Icons.fact_check_outlined,
    _ => Icons.widgets_outlined,
  };

  Widget task(ThemeData theme) {
    final steps = body['steps'] as List? ?? const [];
    final progress = body['progress'];
    final change = (body['change'] as num?)?.toInt();
    double? fraction;
    String? count;
    if (progress is List && progress.length == 2) {
      final done = (progress[0] as num).toInt();
      final total = (progress[1] as num).toInt();
      if (total > 0) fraction = done / total;
      count = '$done/$total';
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final (i, s) in steps.indexed)
          _step(theme, i, (s as Map).cast<String, dynamic>()),
        if (fraction != null || card.state == 'running') ...[
          const SizedBox(height: 8),
          Row(
            children: [
              Expanded(child: LinearProgressIndicator(value: fraction)),
              if (count != null) ...[
                const SizedBox(width: 10),
                Text(count, style: theme.textTheme.labelMedium),
              ],
            ],
          ),
        ],
        if (change != null) ...[
          const SizedBox(height: 8),
          Align(
            alignment: Alignment.centerLeft,
            child: Pill(
              t.change(change),
              color: theme.colorScheme.secondary,
              icon: Icons.history,
            ),
          ),
        ],
      ],
    );
  }

  Widget _step(ThemeData theme, int i, Map<String, dynamic> step) {
    final detail = step['detail'] as String?;
    final shown = open.contains(i);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        InkWell(
          onTap: detail == null
              ? null
              : () => setState(() => shown ? open.remove(i) : open.add(i)),
          child: Padding(
            padding: const EdgeInsets.symmetric(vertical: 4),
            child: Row(
              children: [
                stateIcon(context, step['state'] as String? ?? 'running'),
                const SizedBox(width: 10),
                Expanded(child: Text(step['text'] as String? ?? '')),
                if (detail != null)
                  Icon(
                    shown ? Icons.expand_less : Icons.expand_more,
                    size: 18,
                    color: theme.colorScheme.outline,
                  ),
              ],
            ),
          ),
        ),
        if (shown && detail != null)
          Container(
            margin: const EdgeInsets.only(left: 28, bottom: 4),
            padding: const EdgeInsets.all(8),
            decoration: BoxDecoration(
              color: theme.colorScheme.surfaceContainerHighest,
              borderRadius: BorderRadius.circular(8),
            ),
            child: SelectableText(detail, style: mono.copyWith(fontSize: 12)),
          ),
      ],
    );
  }

  Widget confirmBody(ThemeData theme) {
    final effects = body['effects'] as List? ?? const [];
    final timeout = (body['timeout_s'] as num?)?.toInt();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (body['risk'] == 'high') ...[
          Pill(
            t.highRisk,
            color: theme.colorScheme.error,
            icon: Icons.warning_amber,
          ),
          const SizedBox(height: 8),
        ],
        Text(body['summary'] as String? ?? ''),
        for (final effect in effects)
          Padding(
            padding: const EdgeInsets.only(top: 4, left: 4),
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                const Text('•  '),
                Expanded(child: Text('$effect')),
              ],
            ),
          ),
        if (timeout != null && card.waiting)
          Padding(
            padding: const EdgeInsets.only(top: 8),
            child: Text(
              t.timeout(timeout),
              style: theme.textTheme.bodySmall?.copyWith(
                color: theme.colorScheme.onSurfaceVariant,
              ),
            ),
          ),
      ],
    );
  }

  Widget secretBody(ThemeData theme) {
    final min = (body['min_len'] as num?)?.toInt() ?? 0;
    final saved = body['saved'] == true;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(body['purpose'] as String? ?? ''),
        const SizedBox(height: 10),
        if (saved)
          Pill(t.saved, color: good(context), icon: Icons.lock_outline)
        else if (card.waiting)
          TextField(
            key: Key('secret-${card.id}'),
            controller: secret,
            obscureText: true,
            autocorrect: false,
            enableSuggestions: false,
            enableIMEPersonalizedLearning: false,
            keyboardType: TextInputType.visiblePassword,
            onChanged: (_) => setState(() {}),
            decoration: InputDecoration(
              labelText: body['name'] as String? ?? t.secret,
              helperText: min > 0 ? t.atLeast(min) : null,
              prefixIcon: const Icon(Icons.key),
            ),
          ),
      ],
    );
  }

  Widget choiceBody(ThemeData theme) {
    final multi = body['multi'] == true;
    final options = body['options'] as List? ?? const [];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(body['question'] as String? ?? ''),
        const SizedBox(height: 6),
        for (final o in options.map((o) => (o as Map).cast<String, dynamic>()))
          ListTile(
            key: Key('option-${card.id}-${o['id']}'),
            contentPadding: EdgeInsets.zero,
            dense: true,
            enabled: card.waiting,
            leading: Icon(
              chosen.contains(o['id'])
                  ? (multi ? Icons.check_box : Icons.radio_button_checked)
                  : (multi
                        ? Icons.check_box_outline_blank
                        : Icons.radio_button_unchecked),
              color: theme.colorScheme.primary,
            ),
            title: Text(o['label'] as String? ?? ''),
            subtitle: o['detail'] == null ? null : Text(o['detail'] as String),
            onTap: () => setState(() {
              final id = o['id'] as String;
              if (!multi) {
                chosen
                  ..clear()
                  ..add(id);
              } else if (!chosen.remove(id)) {
                chosen.add(id);
              }
            }),
          ),
      ],
    );
  }

  Widget fileBody(ThemeData theme) {
    final size = (body['size'] as num?)?.toInt() ?? 0;
    final progress = (body['progress'] as num?)?.toInt() ?? 0;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            Icon(
              body['direction'] == 'in' ? Icons.download : Icons.upload,
              size: 18,
            ),
            const SizedBox(width: 8),
            Expanded(
              child: Text(
                body['path'] as String? ?? '',
                style: mono.copyWith(fontSize: 13),
              ),
            ),
            Text(bytes(size), style: theme.textTheme.labelMedium),
          ],
        ),
        if (card.state == 'running') ...[
          const SizedBox(height: 8),
          LinearProgressIndicator(value: size > 0 ? progress / size : null),
        ],
      ],
    );
  }

  Widget checklistBody(ThemeData theme) {
    final items = body['items'] as List? ?? const [];
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final item in items.map((i) => (i as Map).cast<String, dynamic>()))
          Padding(
            padding: const EdgeInsets.symmetric(vertical: 2),
            child: Row(
              children: [
                stateIcon(context, item['state'] as String? ?? 'waiting'),
                const SizedBox(width: 10),
                Expanded(child: Text(item['text'] as String? ?? '')),
                if (item['fix'] is Map)
                  TextButton(
                    key: Key('fix-${card.id}-${item['id']}'),
                    onPressed: busy
                        ? null
                        : () => press(
                            FeedAction((item['fix'] as Map).cast()),
                            item['id'] as String?,
                          ),
                    child: Text(
                      (item['fix'] as Map)['label'] as String? ?? t.fix,
                    ),
                  ),
              ],
            ),
          ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final (label, color) = cardState(context, card.state);
    final content = switch (card.type) {
      'task' => task(theme),
      'confirm' => confirmBody(theme),
      'secret' => secretBody(theme),
      'choice' => choiceBody(theme),
      'file' => fileBody(theme),
      'checklist' => checklistBody(theme),
      _ => const SizedBox.shrink(),
    };
    final buttons = actions;
    return Card(
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(16),
        side: card.waiting
            ? BorderSide(color: theme.colorScheme.tertiary, width: 1.5)
            : BorderSide.none,
      ),
      child: Padding(
        padding: const EdgeInsets.fromLTRB(16, 14, 16, 14),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Tile(icon, size: 32, active: card.state != 'cancelled'),
                const SizedBox(width: 12),
                Expanded(
                  child: Text(
                    card.title,
                    style: theme.textTheme.titleMedium?.copyWith(
                      fontWeight: FontWeight.w600,
                    ),
                  ),
                ),
                Pill(label, color: color),
              ],
            ),
            const SizedBox(height: 12),
            content,
            if (buttons.isNotEmpty ||
                (card.type == 'task' && card.state == 'running')) ...[
              const SizedBox(height: 12),
              Wrap(
                alignment: WrapAlignment.end,
                spacing: 8,
                runSpacing: 8,
                children: [
                  if (card.type == 'task' && card.state == 'running')
                    TextButton.icon(
                      icon: const Icon(Icons.stop),
                      label: Text(t.stop),
                      onPressed: widget.cancel,
                    ),
                  for (final action in buttons) button(action),
                ],
              ),
            ],
          ],
        ),
      ),
    );
  }
}

/// The device at a glance: CPU, temperature, memory, zram, writes, proxy.
class StatusStrip extends StatelessWidget {
  const StatusStrip(this.status, {super.key});

  final FeedStatus status;

  @override
  Widget build(BuildContext context) {
    final t = T.of;
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    Widget stat(IconData icon, String text, {bool warn = false}) => Padding(
      padding: const EdgeInsets.only(right: 16),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(icon, size: 16, color: warn ? colors.error : colors.outline),
          const SizedBox(width: 5),
          Text(
            text,
            style: theme.textTheme.labelMedium?.copyWith(
              color: warn ? colors.error : colors.onSurfaceVariant,
              fontFeatures: const [FontFeature.tabularFigures()],
            ),
          ),
        ],
      ),
    );
    final zram = status.swap.where(
      (s) => (s['device'] as String? ?? '').contains('zram'),
    );
    final temp = status.temp;
    final proxy = status.proxy;
    return Material(
      color: colors.surfaceContainerLow,
      child: SingleChildScrollView(
        scrollDirection: Axis.horizontal,
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 6),
        child: Row(
          children: [
            stat(
              Icons.memory,
              '${t.cpu} ${status.cpu.toStringAsFixed(0)}%',
              warn: status.cpu >= 90,
            ),
            if (temp != null)
              stat(
                Icons.thermostat,
                '${temp.toStringAsFixed(0)}°C',
                warn: temp >= 75,
              ),
            stat(
              Icons.developer_board,
              '${t.memory} ${bytes(status.memUsed)} / ${bytes(status.memTotal)}',
              warn:
                  status.memTotal > 0 && status.memUsed / status.memTotal > .9,
            ),
            for (final z in zram)
              stat(
                Icons.compress,
                'zram ${bytes(z['used'] as num? ?? 0)}'
                '${z['compressed'] == null ? '' : ' → ${bytes(z['compressed'] as num)}'}',
              ),
            stat(Icons.save_outlined, '${t.writes} ${bytes(status.writes)}/s'),
            if (proxy != null)
              stat(
                Icons.vpn_lock_outlined,
                '${t.proxy} ${proxy['reachable'] == true ? t.proxyUp : t.proxyDown}',
                warn: proxy['reachable'] != true,
              ),
            if (status.update != null)
              stat(Icons.system_update_alt, status.update!),
          ],
        ),
      ),
    );
  }
}
