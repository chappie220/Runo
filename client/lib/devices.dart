import 'package:flutter/material.dart';

import 'device.dart';
import 'main.dart';
import 'pair.dart';
import 'src/rust/api/client.dart' as rust;
import 'store.dart';
import 'text.dart';
import 'theme.dart';
import 'widgets.dart';

/// The paired devices, and the Runo devices answering on this network.
class DevicesPage extends StatefulWidget {
  const DevicesPage({super.key, required this.store});

  final Store store;

  @override
  State<DevicesPage> createState() => _DevicesPageState();
}

class _DevicesPageState extends State<DevicesPage> {
  final t = T.of;
  List<rust.PairedDevice> paired = [];
  List<rust.FoundDevice> found = [];
  bool looking = false;

  @override
  void initState() {
    super.initState();
    load();
    look();
  }

  void load() {
    setState(() => paired = rust.pairedDevices(dir: widget.store.dir));
  }

  Future<void> look() async {
    if (looking) return;
    setState(() => looking = true);
    try {
      final answers = await Multicast.hold(
        () => rust.discover(dir: widget.store.dir, seconds: 3),
      );
      if (mounted) setState(() => found = answers);
    } catch (error) {
      if (mounted) complain(context, error);
    } finally {
      if (mounted) setState(() => looking = false);
    }
  }

  Future<void> pair(List<String> addresses) async {
    final done = await Navigator.push<bool>(
      context,
      MaterialPageRoute(
        builder: (_) => PairPage(store: widget.store, addresses: addresses),
      ),
    );
    if (done == true) {
      load();
      look();
    }
  }

  Future<void> pairByAddress() async {
    final field = TextEditingController();
    final address = await showDialog<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(t.addAddress),
        content: TextField(
          controller: field,
          autofocus: true,
          decoration: InputDecoration(labelText: t.address),
          onSubmitted: (value) => Navigator.pop(context, value),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: Text(t.cancel),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, field.text),
            child: Text(t.pair),
          ),
        ],
      ),
    );
    if (address != null && address.trim().isNotEmpty) {
      await pair([address.trim()]);
    }
  }

  Future<void> forget(rust.PairedDevice device) async {
    if (!await confirm(context, t.forgetAsk(device.name), t.forget)) return;
    rust.forgetDevice(dir: widget.store.dir, id: device.id);
    load();
  }

  void open(rust.PairedDevice device) {
    Navigator.push(
      context,
      MaterialPageRoute(
        builder: (_) => DevicePage(store: widget.store, device: device),
      ),
    ).then((_) => load());
  }

  @override
  Widget build(BuildContext context) {
    final unpaired = found.where((d) => !d.paired).toList();
    return Scaffold(
      floatingActionButton: FloatingActionButton.extended(
        icon: const Icon(Icons.add_link),
        label: Text(t.addAddress),
        onPressed: pairByAddress,
      ),
      body: SafeArea(
        child: RefreshIndicator(
          onRefresh: look,
          child: CustomScrollView(
            slivers: [
              SliverToBoxAdapter(
                child: Content(
                  child: Padding(
                    padding: const EdgeInsets.fromLTRB(16, 0, 16, 96),
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: [
                        Padding(
                          padding: const EdgeInsets.fromLTRB(4, 28, 0, 0),
                          child: Row(
                            children: [
                              Expanded(
                                child: Text(
                                  t.devices,
                                  style: Theme.of(context)
                                      .textTheme
                                      .headlineMedium
                                      ?.copyWith(fontWeight: FontWeight.w700),
                                ),
                              ),
                              IconButton.filledTonal(
                                tooltip: t.refresh,
                                icon: const Icon(Icons.refresh),
                                onPressed: looking ? null : look,
                              ),
                            ],
                          ),
                        ),
                        Heading(
                          t.paired,
                          trailing: paired.isEmpty
                              ? null
                              : _Count(paired.length),
                        ),
                        if (paired.isEmpty)
                          Empty(icon: Icons.devices_other, text: t.noneYet)
                        else
                          Cards(
                            children: [
                              for (final device in paired)
                                _PairedCard(
                                  device: device,
                                  nearby: found.any((d) => d.id == device.id),
                                  onOpen: () => open(device),
                                  onForget: () => forget(device),
                                ),
                            ],
                          ),
                        Heading(
                          t.nearby,
                          trailing: looking
                              ? const SizedBox(
                                  width: 16,
                                  height: 16,
                                  child: CircularProgressIndicator(
                                    strokeWidth: 2,
                                  ),
                                )
                              : null,
                        ),
                        if (unpaired.isEmpty)
                          Empty(
                            icon: Icons.wifi_find,
                            text: looking ? t.searching : t.noneNearby,
                          )
                        else
                          Cards(
                            children: [
                              for (final device in unpaired)
                                _FoundCard(
                                  device: device,
                                  onPair: () => pair(device.addresses),
                                ),
                            ],
                          ),
                      ],
                    ),
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _Count extends StatelessWidget {
  const _Count(this.n);

  final int n;

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
      decoration: BoxDecoration(
        color: colors.secondaryContainer,
        borderRadius: BorderRadius.circular(99),
      ),
      child: Text(
        '$n',
        style: Theme.of(context).textTheme.labelMedium
            ?.copyWith(color: colors.onSecondaryContainer),
      ),
    );
  }
}

/// A paired device: whether it answers here, or how it is reached from
/// elsewhere.
class _PairedCard extends StatelessWidget {
  const _PairedCard({
    required this.device,
    required this.nearby,
    required this.onOpen,
    required this.onForget,
  });

  final rust.PairedDevice device;
  final bool nearby;
  final VoidCallback onOpen;
  final VoidCallback onForget;

  @override
  Widget build(BuildContext context) {
    final t = T.of;
    final domain = device.domain;
    return Card(
      child: InkWell(
        onTap: onOpen,
        child: Padding(
          padding: const EdgeInsets.fromLTRB(16, 16, 8, 16),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Tile(Icons.memory, active: nearby),
              const SizedBox(width: 14),
              Expanded(
                child: _Lines(
                  name: device.name,
                  id: device.id,
                  pills: [
                    if (nearby)
                      Pill(t.here, color: good(context))
                    else if (domain != null)
                      Pill(
                        t.remoteAt(domain),
                        color: Theme.of(context).colorScheme.tertiary,
                        icon: Icons.public,
                      )
                    else
                      Pill(t.away, color: state(context, false)),
                  ],
                ),
              ),
              PopupMenuButton<void>(
                tooltip: t.more,
                itemBuilder: (_) => [
                  PopupMenuItem(
                    onTap: onOpen,
                    child: ListTile(
                      leading: const Icon(Icons.open_in_new),
                      title: Text(t.open),
                    ),
                  ),
                  PopupMenuItem(
                    onTap: onForget,
                    child: ListTile(
                      leading: const Icon(Icons.delete_outline),
                      title: Text(t.forget),
                    ),
                  ),
                ],
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// An unpaired device answering on this network.
class _FoundCard extends StatelessWidget {
  const _FoundCard({required this.device, required this.onPair});

  final rust.FoundDevice device;
  final VoidCallback onPair;

  @override
  Widget build(BuildContext context) {
    final t = T.of;
    final colors = Theme.of(context).colorScheme;
    final usable = !device.incompatible;
    return Card(
      child: InkWell(
        onTap: usable ? onPair : null,
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Tile(Icons.wifi_tethering, active: usable && device.pairing),
              const SizedBox(width: 14),
              Expanded(
                child: _Lines(
                  name: device.name,
                  id: device.id,
                  pills: [
                    if (device.incompatible)
                      Pill(
                        t.incompatible,
                        color: colors.error,
                        icon: Icons.error_outline,
                      )
                    else if (device.pairing)
                      Pill(
                        t.pairingOpen,
                        color: colors.primary,
                        icon: Icons.lock_open,
                      )
                    else
                      Pill(
                        t.pairingClosed,
                        color: colors.outline,
                        icon: Icons.lock_outline,
                      ),
                  ],
                ),
              ),
              if (usable) ...[
                const SizedBox(width: 8),
                device.pairing
                    ? FilledButton(onPressed: onPair, child: Text(t.pair))
                    : OutlinedButton(onPressed: onPair, child: Text(t.pair)),
              ],
            ],
          ),
        ),
      ),
    );
  }
}

class _Lines extends StatelessWidget {
  const _Lines({required this.name, required this.id, required this.pills});

  final String name;
  final String id;
  final List<Widget> pills;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          name,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: theme.textTheme.titleMedium?.copyWith(
            fontWeight: FontWeight.w600,
          ),
        ),
        const SizedBox(height: 2),
        Text(
          id,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: mono.copyWith(
            fontSize: 12,
            color: theme.colorScheme.onSurfaceVariant,
          ),
        ),
        const SizedBox(height: 10),
        Wrap(spacing: 6, runSpacing: 6, children: pills),
      ],
    );
  }
}
