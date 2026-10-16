import 'dart:convert';

import 'package:flutter/foundation.dart';

import 'src/rust/api/client.dart' as rust;

/// The Agent as an event stream (Runo 1.0, 4.11): what the device sends on
/// a feed session, as `runo_link::feed` writes it, and what this app keeps
/// of it.

/// A message: the person's, the Agent's, or a note from Runo.
class FeedMessage {
  FeedMessage(this.id, this.role, this.text, this.done);

  final String id;

  /// "you", "agent" or "note".
  final String role;
  String text;
  bool done;
}

/// A card, kept as the device sent it; its body is read where it is drawn.
class FeedCard {
  FeedCard(Map<String, dynamic> json)
    : id = json['id'] as String,
      rev = (json['rev'] as num?)?.toInt() ?? 0,
      title = json['title'] as String? ?? '',
      state = json['state'] as String? ?? 'running',
      body = (json['body'] as Map?)?.cast<String, dynamic>() ?? const {},
      actions = [
        for (final a in json['actions'] as List? ?? const [])
          FeedAction((a as Map).cast<String, dynamic>()),
      ];

  final String id;
  final int rev;
  final String title;

  /// "waiting", "running", "done", "failed" or "cancelled".
  final String state;

  /// With its kind in `type`: "task", "confirm", "secret", "choice",
  /// "file" or "checklist".
  final Map<String, dynamic> body;
  final List<FeedAction> actions;

  String get type => body['type'] as String? ?? '';
  bool get waiting => state == 'waiting';
}

/// A button on a card.
class FeedAction {
  FeedAction(Map<String, dynamic> json)
    : id = json['id'] as String,
      label = json['label'] as String? ?? json['id'] as String,
      style = json['style'] as String? ?? 'normal';

  const FeedAction.of(this.id, this.label, [this.style = 'primary']);

  /// What the answer carries.
  final String id;
  final String label;

  /// "primary", "normal" or "danger".
  final String style;
}

/// The device's state.
class FeedStatus {
  FeedStatus(Map<String, dynamic> json)
    : cpu = (json['cpu_percent'] as num?)?.toDouble() ?? 0,
      temp = (json['temp_c'] as num?)?.toDouble(),
      memUsed = _pair(json['mem'], 0),
      memTotal = _pair(json['mem'], 1),
      swap = [
        for (final s in json['swap'] as List? ?? const [])
          (s as Map).cast<String, dynamic>(),
      ],
      writes = (json['disk_write_bps'] as num?)?.toInt() ?? 0,
      proxy = (json['proxy'] as Map?)?.cast<String, dynamic>(),
      update = json['update'] as String?;

  final double cpu;
  final double? temp;
  final int memUsed;
  final int memTotal;

  /// `device`, `size`, `used` and, for zram, `compressed`.
  final List<Map<String, dynamic>> swap;

  /// Bytes per second written to the data partition.
  final int writes;

  /// `address` and `reachable`.
  final Map<String, dynamic>? proxy;
  final String? update;

  static int _pair(Object? value, int i) =>
      value is List && value.length > i ? (value[i] as num).toInt() : 0;
}

/// What a feed showed: its messages and cards in the order they first
/// came, whether a turn runs, the latest status, and the number of the
/// last event, which a client coming back attaches from.
class FeedLog extends ChangeNotifier {
  /// Each a [FeedMessage] or a [FeedCard].
  final List<Object> items = [];
  final _messages = <String, FeedMessage>{};
  final _cards = <String, int>{};
  bool busy = false;
  FeedStatus? status;

  /// The highest event number seen. Events caught up after a return come
  /// each with the number of its own last change, not in order, so it is
  /// the highest, not the last.
  int seq = 0;

  FeedCard? card(String id) {
    final i = _cards[id];
    return i == null ? null : items[i] as FeedCard;
  }

  /// Takes in one event, as JSON.
  void add(String json) => apply((jsonDecode(json) as Map).cast());

  void apply(Map<String, dynamic> event) {
    final n = (event['seq'] as num?)?.toInt() ?? 0;
    if (n > seq) seq = n;
    switch (event['kind']) {
      case 'message':
        final id = event['id'] as String;
        final text = event['text'] as String? ?? '';
        final done = event['done'] as bool? ?? false;
        final kept = _messages[id];
        if (kept == null) {
          final message = FeedMessage(
            id,
            event['role'] as String? ?? 'agent',
            text,
            done,
          );
          _messages[id] = message;
          items.add(message);
        } else {
          kept.text = event['append'] == true ? kept.text + text : text;
          kept.done = done;
        }
      case 'card':
        final card = FeedCard((event['card'] as Map).cast());
        final i = _cards[card.id];
        if (i == null) {
          _cards[card.id] = items.length;
          items.add(card);
        } else if ((items[i] as FeedCard).rev < card.rev) {
          items[i] = card;
        }
      case 'status':
        status = FeedStatus((event['status'] as Map).cast());
      case 'turn':
        busy = event['busy'] as bool? ?? false;
      default:
        // A kind of event from a newer device: nothing to show for it.
        return;
    }
    notifyListeners();
  }
}

/// A feed session as the Agent page talks to it; [RustFeed] on a device,
/// something else in tests.
abstract class FeedLink {
  /// Its events, then one frame with `ended`.
  Stream<rust.FeedFrame> events();
  Future<void> send(String text);
  Future<void> answer(String card, String action, String? value);

  /// The turn, or the task on `card`.
  Future<void> cancel(String? card);
  Future<void> detach();
}

/// Opens the feed again from after event `since`.
typedef OpenFeed = Future<FeedLink> Function(int since);

class RustFeed implements FeedLink {
  RustFeed(this.feed);

  final rust.Feed feed;

  @override
  Stream<rust.FeedFrame> events() => feed.events();

  @override
  Future<void> send(String text) => feed.send(text: text, attachments: []);

  @override
  Future<void> answer(String card, String action, String? value) =>
      feed.answer(card: card, action: action, value: value);

  @override
  Future<void> cancel(String? card) => feed.cancel(card: card);

  @override
  Future<void> detach() => feed.detach();
}

/// The Agent's feed on a device: the session `attach`, or the running one
/// or a new one; once open, always that session. [connection] gives the
/// connection to use, a new one when the last was lost.
OpenFeed rustFeed(
  Future<rust.Connection?> Function() connection, {
  BigInt? attach,
}) {
  var session = attach;
  return (since) async {
    final c = await connection();
    if (c == null) throw StateError('not connected');
    final feed = await c.feed(attach: session, since: BigInt.from(since));
    session = feed.session;
    return RustFeed(feed);
  };
}

/// Bytes as a person reads them.
String bytes(num n) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  var value = n.toDouble();
  var unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  final digits = unit == 0 || value >= 100 ? 0 : 1;
  return '${value.toStringAsFixed(digits)} ${units[unit]}';
}
