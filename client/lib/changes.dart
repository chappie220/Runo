import 'dart:async';

import 'package:flutter/material.dart';

import 'main.dart';
import 'src/rust/api/client.dart' as rust;
import 'text.dart';

/// Above [child]: the device's configuration change waiting for this
/// client to confirm it, with Keep and Put back (design 4.2). Reaching it
/// to keep the change is what shows the change did not cut the client off.
class ChangeBanner extends StatefulWidget {
  const ChangeBanner({
    super.key,
    required this.connection,
    required this.child,
  });

  final rust.Connection connection;
  final Widget child;

  @override
  State<ChangeBanner> createState() => _ChangeBannerState();
}

class _ChangeBannerState extends State<ChangeBanner> {
  final t = T.of;
  Timer? timer;
  rust.ChangeInfo? waiting;
  bool busy = false;

  @override
  void initState() {
    super.initState();
    poll();
    timer = Timer.periodic(const Duration(seconds: 1), (tick) {
      // Ask the device every few seconds; count down in between.
      if (tick.tick % 3 == 0) {
        poll();
      } else if (waiting != null && mounted) {
        setState(() {});
      }
    });
  }

  Future<void> poll() async {
    if (busy || widget.connection.closed() != null) return;
    try {
      final changes = await widget.connection.unconfirmed();
      if (mounted) {
        setState(() => waiting = changes.isEmpty ? null : changes.first);
      }
    } catch (_) {
      // An older device, or the connection is going: nothing to show.
    }
  }

  Future<void> settle(rust.ChangeInfo change, bool keep) async {
    setState(() => busy = true);
    try {
      await widget.connection.confirm(id: change.id, keep: keep);
      if (!mounted) return;
      setState(() => waiting = null);
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text(keep ? t.kept : t.putBackDone)));
    } catch (e) {
      if (mounted) complain(context, e);
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  void dispose() {
    timer?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final change = waiting;
    if (change == null) return widget.child;
    final left = change.until - DateTime.now().millisecondsSinceEpoch ~/ 1000;
    return Column(
      children: [
        MaterialBanner(
          leading: const Icon(Icons.warning_amber),
          content: Text(
            t.waiting(change.id.toInt(), change.summary, left < 0 ? 0 : left),
          ),
          actions: [
            TextButton(
              onPressed: busy ? null : () => settle(change, false),
              child: Text(t.putBack),
            ),
            FilledButton(
              onPressed: busy ? null : () => settle(change, true),
              child: Text(t.keep),
            ),
          ],
        ),
        Expanded(child: widget.child),
      ],
    );
  }
}
