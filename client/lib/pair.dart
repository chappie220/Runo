import 'dart:io';

import 'package:flutter/material.dart';

import 'main.dart';
import 'src/rust/api/client.dart' as rust;
import 'store.dart';
import 'text.dart';

/// Pairs with the device at `addresses`: shows the code both sides
/// derive, then gives the device password once they match. Pops `true`
/// once paired.
class PairPage extends StatefulWidget {
  const PairPage({super.key, required this.store, required this.addresses});

  final Store store;
  final List<String> addresses;

  @override
  State<PairPage> createState() => _PairPageState();
}

class _PairPageState extends State<PairPage> {
  final t = T.of;
  final password = TextEditingController();
  late final name = TextEditingController(text: Platform.localHostname);
  rust.Pairing? pairing;
  String? error;
  bool busy = false;

  @override
  void initState() {
    super.initState();
    start();
  }

  @override
  void dispose() {
    password.dispose();
    name.dispose();
    super.dispose();
  }

  Future<void> start() async {
    setState(() {
      error = null;
      pairing = null;
    });
    try {
      final started = await rust.Pairing.start(
        dir: widget.store.dir,
        identity: widget.store.identity,
        addresses: widget.addresses,
      );
      if (!mounted) return;
      setState(() => pairing = started);
      if (started.code == null) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: Text(t.alreadyTrusted)));
        Navigator.pop(context, true);
      }
    } catch (e) {
      if (mounted) setState(() => error = message(e));
    }
  }

  Future<void> finish() async {
    final pairing = this.pairing;
    if (pairing == null || password.text.isEmpty) return;
    setState(() => busy = true);
    try {
      await pairing.finish(
        password: password.text,
        clientName: name.text.trim(),
      );
      password.clear();
      if (!mounted) return;
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text(t.pairedDone)));
      Navigator.pop(context, true);
    } catch (e) {
      password.clear();
      if (mounted) complain(context, e);
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final pairing = this.pairing;
    final code = pairing?.code;
    final Widget body;
    if (error != null) {
      body = Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(error!, textAlign: TextAlign.center),
          const SizedBox(height: 16),
          FilledButton(onPressed: start, child: Text(t.reconnect)),
        ],
      );
    } else if (pairing == null || code == null) {
      body = Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          const CircularProgressIndicator(),
          const SizedBox(height: 16),
          Text(t.connecting),
        ],
      );
    } else {
      body = Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(
            '${pairing.deviceName}\n${pairing.deviceId}',
            textAlign: TextAlign.center,
            style: theme.textTheme.titleMedium,
          ),
          const SizedBox(height: 24),
          Text(
            '${code.substring(0, 3)} ${code.substring(3)}',
            textAlign: TextAlign.center,
            style: theme.textTheme.displayMedium?.copyWith(
              fontFeatures: const [FontFeature.tabularFigures()],
              letterSpacing: 4,
            ),
          ),
          const SizedBox(height: 12),
          Text(t.compareCode, textAlign: TextAlign.center),
          const SizedBox(height: 24),
          TextField(
            controller: password,
            obscureText: true,
            autofocus: true,
            enableSuggestions: false,
            autocorrect: false,
            decoration: InputDecoration(
              labelText: t.password,
              border: const OutlineInputBorder(),
            ),
            onSubmitted: (_) => finish(),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: name,
            decoration: InputDecoration(
              labelText: t.clientName,
              border: const OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 20),
          FilledButton(
            onPressed: busy ? null : finish,
            child: Text(t.codesMatch),
          ),
        ],
      );
    }
    return Scaffold(
      appBar: AppBar(title: Text(t.pair)),
      body: Center(
        child: SingleChildScrollView(
          padding: const EdgeInsets.all(24),
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 420),
            child: body,
          ),
        ),
      ),
    );
  }
}
