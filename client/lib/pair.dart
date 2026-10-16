import 'dart:io';

import 'package:flutter/material.dart';

import 'problem.dart';
import 'src/rust/api/client.dart' as rust;
import 'store.dart';
import 'text.dart';
import 'theme.dart';
import 'widgets.dart';

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
  Problem? error;

  /// Why the password was not taken, shown under it.
  String? refused;

  /// The code was compared and found the same.
  bool compared = false;
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
      compared = false;
      refused = null;
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
      if (mounted) setState(() => error = Problem.of(e));
    }
  }

  Future<void> finish() async {
    final pairing = this.pairing;
    if (pairing == null || password.text.isEmpty) return;
    setState(() {
      busy = true;
      refused = null;
    });
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
      if (mounted) setState(() => refused = message(e));
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final pairing = this.pairing;
    final code = pairing?.code;
    final int step;
    final Widget body;
    if (error != null) {
      step = 0;
      body = ProblemView(
        problem: error!,
        icon: Icons.link_off,
        retry: start,
        retryLabel: t.reconnect,
      );
    } else if (pairing == null || code == null) {
      step = 0;
      body = Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          const CircularProgressIndicator(),
          const SizedBox(height: 16),
          Text(t.connecting),
        ],
      );
    } else if (!compared) {
      step = 1;
      body = _compare(pairing, code);
    } else {
      step = 2;
      body = _password(pairing);
    }
    return Scaffold(
      appBar: AppBar(title: Text(t.pair)),
      body: SafeArea(
        child: Column(
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(24, 8, 24, 0),
              child: ConstrainedBox(
                constraints: const BoxConstraints(maxWidth: 520),
                child: _Steps(
                  current: step,
                  labels: [t.stepConnect, t.stepCode, t.stepPassword],
                ),
              ),
            ),
            Expanded(
              child: error != null
                  ? body
                  : Center(
                      child: SingleChildScrollView(
                        padding: const EdgeInsets.all(24),
                        child: ConstrainedBox(
                          constraints: const BoxConstraints(maxWidth: 440),
                          child: AnimatedSwitcher(
                            duration: const Duration(milliseconds: 200),
                            child: KeyedSubtree(
                              key: ValueKey(step),
                              child: body,
                            ),
                          ),
                        ),
                      ),
                    ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _device(rust.Pairing pairing) {
    final theme = Theme.of(context);
    return Row(
      children: [
        const Tile(Icons.memory),
        const SizedBox(width: 14),
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                pairing.deviceName,
                style: theme.textTheme.titleMedium?.copyWith(
                  fontWeight: FontWeight.w600,
                ),
              ),
              Text(
                pairing.deviceId,
                style: mono.copyWith(
                  fontSize: 12,
                  color: theme.colorScheme.onSurfaceVariant,
                ),
              ),
            ],
          ),
        ),
      ],
    );
  }

  Widget _compare(rust.Pairing pairing, String code) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _device(pairing),
        const SizedBox(height: 24),
        Container(
          padding: const EdgeInsets.fromLTRB(20, 20, 20, 24),
          decoration: BoxDecoration(
            color: colors.primaryContainer,
            borderRadius: BorderRadius.circular(24),
          ),
          child: Column(
            children: [
              Text(
                t.codeTitle,
                style: theme.textTheme.labelLarge?.copyWith(
                  color: colors.onPrimaryContainer,
                ),
              ),
              const SizedBox(height: 16),
              FittedBox(child: _Code(code)),
            ],
          ),
        ),
        const SizedBox(height: 16),
        Text(
          t.compareCode,
          textAlign: TextAlign.center,
          style: theme.textTheme.bodyMedium?.copyWith(
            color: colors.onSurfaceVariant,
          ),
        ),
        const SizedBox(height: 24),
        FilledButton.icon(
          icon: const Icon(Icons.check),
          label: Text(t.codesSame),
          onPressed: () => setState(() => compared = true),
        ),
        const SizedBox(height: 8),
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: Text(t.codesDiffer),
        ),
      ],
    );
  }

  Widget _password(rust.Pairing pairing) {
    final theme = Theme.of(context);
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _device(pairing),
        const SizedBox(height: 24),
        Text(t.passwordAbout, style: theme.textTheme.bodyLarge),
        const SizedBox(height: 16),
        TextField(
          controller: password,
          obscureText: true,
          autofocus: true,
          enableSuggestions: false,
          autocorrect: false,
          decoration: InputDecoration(
            labelText: t.password,
            prefixIcon: const Icon(Icons.key),
            errorText: refused,
            errorMaxLines: 4,
          ),
          onSubmitted: (_) => finish(),
        ),
        const SizedBox(height: 12),
        TextField(
          controller: name,
          decoration: InputDecoration(
            labelText: t.clientName,
            prefixIcon: const Icon(Icons.badge_outlined),
          ),
        ),
        const SizedBox(height: 24),
        FilledButton(
          onPressed: busy ? null : finish,
          child: busy
              ? const SizedBox(
                  width: 20,
                  height: 20,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              : Text(t.pair),
        ),
        const SizedBox(height: 8),
        TextButton(
          onPressed: busy ? null : () => setState(() => compared = false),
          child: Text(t.back),
        ),
      ],
    );
  }
}

/// The six digits, one to a box, in two groups of three.
class _Code extends StatelessWidget {
  const _Code(this.code);

  final String code;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    Widget digit(String d) => Container(
      width: 48,
      height: 60,
      margin: const EdgeInsets.symmetric(horizontal: 3),
      alignment: Alignment.center,
      decoration: BoxDecoration(
        color: colors.surface,
        borderRadius: BorderRadius.circular(12),
      ),
      child: Text(
        d,
        style: mono.copyWith(
          fontSize: 34,
          fontWeight: FontWeight.w700,
          color: colors.primary,
        ),
      ),
    );
    final digits = code.split('');
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        for (final d in digits.take(3)) digit(d),
        const SizedBox(width: 14),
        for (final d in digits.skip(3)) digit(d),
      ],
    );
  }
}

/// Where pairing has got to: connect, compare the code, give the password.
class _Steps extends StatelessWidget {
  const _Steps({required this.current, required this.labels});

  final int current;
  final List<String> labels;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    final row = <Widget>[];
    for (var i = 0; i < labels.length; i++) {
      final done = i < current;
      final now = i == current;
      if (i > 0) {
        row.add(
          Expanded(
            child: Container(
              height: 2,
              margin: const EdgeInsets.only(bottom: 22),
              color: done || now ? colors.primary : colors.outlineVariant,
            ),
          ),
        );
      }
      row.add(
        Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Container(
              width: 28,
              height: 28,
              alignment: Alignment.center,
              decoration: BoxDecoration(
                shape: BoxShape.circle,
                color: done || now ? colors.primary : Colors.transparent,
                border: Border.all(
                  color: done || now ? colors.primary : colors.outline,
                  width: 2,
                ),
              ),
              child: done
                  ? Icon(Icons.check, size: 16, color: colors.onPrimary)
                  : Text(
                      '${i + 1}',
                      style: theme.textTheme.labelMedium?.copyWith(
                        color: now ? colors.onPrimary : colors.outline,
                        fontWeight: FontWeight.w700,
                      ),
                    ),
            ),
            const SizedBox(height: 6),
            Text(
              labels[i],
              style: theme.textTheme.labelMedium?.copyWith(
                color: now ? colors.onSurface : colors.onSurfaceVariant,
                fontWeight: now ? FontWeight.w600 : null,
              ),
            ),
          ],
        ),
      );
    }
    return Row(crossAxisAlignment: CrossAxisAlignment.center, children: row);
  }
}
