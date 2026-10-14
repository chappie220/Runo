import 'package:flutter/material.dart';

import 'device.dart';
import 'main.dart';
import 'pair.dart';
import 'src/rust/api/client.dart' as rust;
import 'store.dart';
import 'text.dart';

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
    final theme = Theme.of(context);
    final unpaired = found.where((d) => !d.paired).toList();
    return Scaffold(
      appBar: AppBar(
        title: Text(t.devices),
        actions: [
          IconButton(
            tooltip: t.addAddress,
            icon: const Icon(Icons.add_link),
            onPressed: pairByAddress,
          ),
          IconButton(
            tooltip: t.refresh,
            icon: const Icon(Icons.refresh),
            onPressed: looking ? null : look,
          ),
        ],
      ),
      body: RefreshIndicator(
        onRefresh: look,
        child: ListView(
          children: [
            _Heading(t.paired),
            if (paired.isEmpty)
              ListTile(
                title: Text(t.noneYet, style: theme.textTheme.bodySmall),
              ),
            for (final device in paired)
              ListTile(
                leading: Icon(
                  found.any((d) => d.id == device.id)
                      ? Icons.memory
                      : Icons.memory_outlined,
                ),
                title: Text(device.name),
                subtitle: Text(device.id),
                onTap: () => open(device),
                trailing: IconButton(
                  tooltip: t.forget,
                  icon: const Icon(Icons.delete_outline),
                  onPressed: () => forget(device),
                ),
              ),
            _Heading(t.nearby),
            if (looking) const LinearProgressIndicator(),
            if (looking && unpaired.isEmpty) ListTile(title: Text(t.searching)),
            if (!looking && unpaired.isEmpty)
              ListTile(
                title: Text(t.noneNearby, style: theme.textTheme.bodySmall),
              ),
            for (final device in unpaired)
              ListTile(
                leading: const Icon(Icons.wifi_tethering),
                title: Text(device.name),
                subtitle: Text(
                  device.incompatible
                      ? t.incompatible
                      : '${device.id} · ${device.pairing ? t.pairingOpen : t.pairingClosed}',
                ),
                enabled: !device.incompatible,
                onTap: () => pair(device.addresses),
              ),
          ],
        ),
      ),
    );
  }
}

class _Heading extends StatelessWidget {
  const _Heading(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 20, 16, 4),
      child: Text(
        text,
        style: theme.textTheme.titleSmall?.copyWith(
          color: theme.colorScheme.primary,
        ),
      ),
    );
  }
}
