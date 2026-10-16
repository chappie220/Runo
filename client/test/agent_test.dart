import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:runo_app/agent.dart';
import 'package:runo_app/feed.dart';
import 'package:runo_app/src/rust/api/client.dart' as rust;

/// A feed session the test speaks for.
class FakeFeed implements FeedLink {
  final frames = StreamController<rust.FeedFrame>();
  final sent = <String>[];
  final answers = <(String, String, String?)>[];
  final cancels = <String?>[];
  bool detached = false;

  void event(Map<String, dynamic> event) =>
      frames.add(rust.FeedFrame(event: jsonEncode(event), ended: false));

  void lose() => frames.add(const rust.FeedFrame(ended: true, error: 'gone'));

  @override
  Stream<rust.FeedFrame> events() => frames.stream;

  @override
  Future<void> send(String text) async => sent.add(text);

  @override
  Future<void> answer(String card, String action, String? value) async =>
      answers.add((card, action, value));

  @override
  Future<void> cancel(String? card) async => cancels.add(card);

  @override
  Future<void> detach() async => detached = true;
}

Map<String, dynamic> message(
  int seq,
  String id,
  String text, {
  String role = 'agent',
  bool append = true,
  bool done = false,
}) => {
  'seq': seq,
  'kind': 'message',
  'id': id,
  'role': role,
  'text': text,
  'append': append,
  'done': done,
};

Map<String, dynamic> card(
  int seq,
  String id,
  Map<String, dynamic> body, {
  int rev = 1,
  String state = 'waiting',
  List<Map<String, dynamic>> actions = const [],
  String title = 'Card',
}) => {
  'seq': seq,
  'kind': 'card',
  'card': {
    'id': id,
    'rev': rev,
    'title': title,
    'state': state,
    'body': body,
    'actions': actions,
  },
};

void main() {
  late List<FakeFeed> feeds;
  late List<int> opened;
  String? clipboard;

  Future<FakeFeed> start(WidgetTester tester) async {
    feeds = [];
    opened = [];
    clipboard = null;
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          clipboard = (call.arguments as Map)['text'] as String?;
        }
        return null;
      },
    );
    await tester.pumpWidget(
      MaterialApp(
        home: AgentPage(
          title: 'Agent',
          open: (since) async {
            opened.add(since);
            final feed = FakeFeed();
            feeds.add(feed);
            return feed;
          },
        ),
      ),
    );
    await tester.pump();
    return feeds.single;
  }

  testWidgets('a reply streams in, is replaced, and its code is copied', (
    tester,
  ) async {
    final feed = await start(tester);
    feed.event(message(1, 'u1', 'hi', role: 'you', append: false, done: true));
    feed.event(message(2, 'm1', 'Hel'));
    await tester.pump();
    expect(find.text('hi'), findsOneWidget);
    expect(find.text('Hel'), findsOneWidget);
    // Still arriving.
    expect(find.byType(CircularProgressIndicator), findsOneWidget);

    feed.event(message(3, 'm1', 'lo **world**'));
    await tester.pump();
    expect(find.text('Hello world'), findsOneWidget);
    expect(find.text('Hel'), findsNothing);

    feed.event(
      message(
        4,
        'm1',
        'Run:\n\n```sh\napt install htop\n```',
        append: false,
        done: true,
      ),
    );
    await tester.pump();
    expect(find.text('Hello world'), findsNothing);
    expect(find.text('apt install htop'), findsOneWidget);
    expect(find.text('sh'), findsOneWidget);
    expect(find.byType(CircularProgressIndicator), findsNothing);

    await tester.tap(find.byIcon(Icons.copy));
    await tester.pump();
    expect(clipboard, 'apt install htop');
  });

  testWidgets('cards are answered with their actions and values', (
    tester,
  ) async {
    final feed = await start(tester);
    feed.event(
      card(
        1,
        'c1',
        {
          'type': 'confirm',
          'summary': 'Restart the device',
          'effects': ['Connections drop for a minute'],
          'risk': 'high',
          'timeout_s': 60,
        },
        actions: [
          {'id': 'yes', 'label': 'Restart', 'style': 'danger'},
          {'id': 'no', 'label': 'Keep running', 'style': 'normal'},
        ],
      ),
    );
    await tester.pump();
    expect(find.text('Connections drop for a minute'), findsOneWidget);
    final danger = tester.widget<FilledButton>(
      find.byKey(const Key('action-c1-yes')),
    );
    final context = tester.element(find.byType(AgentPage));
    expect(
      danger.style?.backgroundColor?.resolve({}),
      Theme.of(context).colorScheme.error,
    );
    await tester.tap(find.byKey(const Key('action-c1-yes')));
    await tester.pump();
    expect(feed.answers.last, ('c1', 'yes', null));

    // A secret goes with its button and leaves the field empty.
    feed.event(
      card(2, 's1', {
        'type': 'secret',
        'name': 'cloudflare_token',
        'purpose': 'For the tunnel',
        'min_len': 4,
        'saved': false,
      }),
    );
    await tester.pump();
    final field = find.byKey(const Key('secret-s1'));
    expect(
      tester
          .widget<EditableText>(
            find.descendant(of: field, matching: find.byType(EditableText)),
          )
          .obscureText,
      isTrue,
    );
    await tester.enterText(field, 'cfu');
    await tester.pump();
    // Too short for its button.
    expect(
      tester
          .widget<FilledButton>(find.byKey(const Key('action-s1-save')))
          .onPressed,
      isNull,
    );
    await tester.enterText(field, 'cfut_secret');
    await tester.pump();
    await tester.tap(find.byKey(const Key('action-s1-save')));
    await tester.pump();
    expect(feed.answers.last, ('s1', 'save', 'cfut_secret'));
    expect(tester.widget<TextField>(field).controller!.text, isEmpty);
    feed.event(
      card(
        3,
        's1',
        {
          'type': 'secret',
          'name': 'cloudflare_token',
          'purpose': 'For the tunnel',
          'saved': true,
        },
        rev: 2,
        state: 'done',
      ),
    );
    await tester.pump();
    expect(field, findsNothing);
    expect(find.text('Saved'), findsOneWidget);

    // A choice carries the options picked.
    feed.event(
      card(4, 'k1', {
        'type': 'choice',
        'question': 'Which model?',
        'options': [
          {'id': 'small', 'label': 'Small'},
          {'id': 'large', 'label': 'Large', 'detail': 'Slower'},
        ],
      }),
    );
    await tester.pump();
    await tester.tap(find.byKey(const Key('option-k1-large')));
    await tester.pump();
    await tester.tap(find.byKey(const Key('action-k1-choose')));
    await tester.pump();
    expect(feed.answers.last, ('k1', 'choose', 'large'));

    // A checklist item's "handle it".
    feed.event(
      card(5, 'l1', {
        'type': 'checklist',
        'items': [
          {'id': 'time', 'text': 'Clock', 'state': 'done'},
          {
            'id': 'proxy',
            'text': 'Proxy',
            'state': 'failed',
            'fix': {'id': 'fix_proxy', 'label': 'Handle it'},
          },
        ],
      }, state: 'running'),
    );
    await tester.pump();
    await tester.ensureVisible(find.byKey(const Key('fix-l1-proxy')));
    await tester.tap(find.byKey(const Key('fix-l1-proxy')));
    await tester.pump();
    expect(feed.answers.last, ('l1', 'fix_proxy', 'proxy'));
  });

  testWidgets('a task shows its steps and progress, an older rev loses', (
    tester,
  ) async {
    final feed = await start(tester);
    Map<String, dynamic> task(int seq, int rev, String state, int done) => card(
      seq,
      't1',
      {
        'type': 'task',
        'steps': [
          {'text': 'Download htop', 'state': 'done'},
          {'text': 'Install htop', 'state': state, 'detail': 'apt-get'},
        ],
        'progress': [done, 2],
      },
      rev: rev,
      state: state,
      title: 'Install htop',
    );
    feed.event(task(1, 2, 'running', 1));
    feed.event(task(2, 1, 'waiting', 0));
    await tester.pump();
    expect(find.text('Download htop'), findsOneWidget);
    expect(find.text('1/2'), findsOneWidget);
    expect(
      tester
          .widget<LinearProgressIndicator>(find.byType(LinearProgressIndicator))
          .value,
      .5,
    );
    await tester.tap(find.text('Install htop').last);
    await tester.pump();
    expect(find.text('apt-get'), findsOneWidget);
    await tester.tap(find.text('Stop'));
    await tester.pump();
    expect(feed.cancels, ['t1']);
  });

  testWidgets('Enter sends, Shift+Enter breaks the line, stop while busy', (
    tester,
  ) async {
    final feed = await start(tester);
    final input = find.byKey(const Key('feed-input'));
    await tester.tap(input);
    const pasted = 'line one\n  line two\n\nline four';
    await tester.enterText(input, pasted);
    await tester.sendKeyDownEvent(LogicalKeyboardKey.shiftLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.shiftLeft);
    await tester.pump();
    expect(tester.widget<TextField>(input).controller!.text, '$pasted\n');
    expect(feed.sent, isEmpty);

    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.pump();
    expect(feed.sent, ['$pasted\n']);
    expect(tester.widget<TextField>(input).controller!.text, isEmpty);

    expect(find.byKey(const Key('feed-stop')), findsNothing);
    feed.event({'seq': 1, 'kind': 'turn', 'busy': true});
    await tester.pump();
    await tester.tap(find.byKey(const Key('feed-stop')));
    await tester.pump();
    expect(feed.cancels, [null]);
    feed.event({'seq': 2, 'kind': 'turn', 'busy': false});
    await tester.pump();
    expect(find.byKey(const Key('feed-stop')), findsNothing);
  });

  testWidgets('the status strip shows the device', (tester) async {
    final feed = await start(tester);
    feed.event({
      'seq': 1,
      'kind': 'status',
      'status': {
        'cpu_percent': 12.4,
        'temp_c': 51.0,
        'mem': [1073741824, 4294967296],
        'swap': [
          {
            'device': '/dev/zram0',
            'size': 2147483648,
            'used': 104857600,
            'compressed': 31457280,
          },
        ],
        'disk_write_bps': 1536,
        'proxy': {'address': '127.0.0.1:7890', 'reachable': true},
      },
    });
    await tester.pump();
    expect(find.text('CPU 12%'), findsOneWidget);
    expect(find.text('51°C'), findsOneWidget);
    expect(find.text('Memory 1.0 GB / 4.0 GB'), findsOneWidget);
    expect(find.text('zram 100 MB → 30.0 MB'), findsOneWidget);
    expect(find.text('Writes 1.5 KB/s'), findsOneWidget);
    expect(find.text('Proxy reachable'), findsOneWidget);
  });

  testWidgets('a lost connection attaches again from the last event', (
    tester,
  ) async {
    final first = await start(tester);
    first.event(message(5, 'm1', 'Hel'));
    first.event(message(3, 'm0', 'earlier', append: false, done: true));
    await tester.pump();
    first.lose();
    await tester.pump();
    expect(find.text('Reconnecting…'), findsOneWidget);
    await tester.pump(const Duration(seconds: 3));
    expect(opened, [0, 5]);

    // The device sends the message whole again, then goes on.
    final second = feeds.last;
    second.event(message(6, 'm1', 'Hello', append: false));
    second.event(message(7, 'm1', '!'));
    await tester.pump();
    expect(find.text('Hello!'), findsOneWidget);
    expect(find.text('Reconnecting…'), findsNothing);
  });

  test('the log keeps the highest number and the newest card', () {
    final log = FeedLog();
    log.apply(card(4, 'c', {'type': 'task', 'steps': []}, rev: 3));
    log.apply(card(2, 'c', {'type': 'task', 'steps': []}, rev: 2));
    log.apply({'seq': 9, 'kind': 'future_thing'});
    expect(log.seq, 9);
    expect(log.card('c')!.rev, 3);
    expect(log.items, hasLength(1));
  });
}
