import 'dart:io';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';

import 'main.dart';
import 'src/rust/api/client.dart' as rust;
import 'store.dart';
import 'terminal.dart';
import 'text.dart';

/// One paired device: its sessions, status, logs, files and clients.
class DevicePage extends StatefulWidget {
  const DevicePage({super.key, required this.store, required this.device});

  final Store store;
  final rust.PairedDevice device;

  @override
  State<DevicePage> createState() => _DevicePageState();
}

class _DevicePageState extends State<DevicePage> {
  final t = T.of;
  rust.Connection? connection;
  String? error;
  int generation = 0;

  @override
  void initState() {
    super.initState();
    connect();
  }

  Future<void> connect() async {
    setState(() {
      connection = null;
      error = null;
    });
    try {
      final opened = await Multicast.hold(
        () => rust.Connection.open(
          dir: widget.store.dir,
          identity: widget.store.identity,
          id: widget.device.id,
        ),
      );
      if (mounted) {
        setState(() {
          connection = opened;
          generation++;
        });
      }
    } catch (e) {
      if (mounted) setState(() => error = message(e));
    }
  }

  /// Runs `call`, and offers to reconnect when the connection is gone.
  Future<R?> guard<R>(Future<R> Function(rust.Connection) call) async {
    final connection = this.connection;
    if (connection == null) return null;
    try {
      return await call(connection);
    } catch (e) {
      if (!mounted) return null;
      final closed = connection.closed();
      if (closed != null) {
        setState(() => error = '${t.closed}: $closed');
      } else {
        complain(context, e);
      }
      return null;
    }
  }

  @override
  Widget build(BuildContext context) {
    final connection = this.connection;
    final tabs = [
      (Icons.terminal, t.sessions),
      (Icons.info_outline, t.status),
      (Icons.receipt_long, t.logs),
      (Icons.folder_outlined, t.files),
      (Icons.devices, t.clients),
    ];
    final Widget body;
    if (error != null) {
      body = Center(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(error!, textAlign: TextAlign.center),
              const SizedBox(height: 16),
              FilledButton(onPressed: connect, child: Text(t.reconnect)),
            ],
          ),
        ),
      );
    } else if (connection == null) {
      body = Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            const CircularProgressIndicator(),
            const SizedBox(height: 16),
            Text(t.connecting),
          ],
        ),
      );
    } else {
      body = TabBarView(
        key: ValueKey(generation),
        children: [
          _Sessions(page: this, connection: connection),
          _Text(load: () => guard((c) => c.status(lang: t.lang))),
          _Text(load: () => guard((c) => c.logs(lang: t.lang)), tail: true),
          _Files(page: this),
          _Clients(page: this),
        ],
      );
    }
    return DefaultTabController(
      length: tabs.length,
      child: Scaffold(
        appBar: AppBar(
          title: Text(connection?.deviceName ?? widget.device.name),
          bottom: connection == null
              ? null
              : TabBar(
                  isScrollable: true,
                  tabAlignment: TabAlignment.start,
                  tabs: [
                    for (final (icon, label) in tabs)
                      Tab(icon: Icon(icon), text: label),
                  ],
                ),
        ),
        body: body,
      ),
    );
  }
}

class _Sessions extends StatefulWidget {
  const _Sessions({required this.page, required this.connection});

  final _DevicePageState page;
  final rust.Connection connection;

  @override
  State<_Sessions> createState() => _SessionsState();
}

class _SessionsState extends State<_Sessions> {
  final t = T.of;
  List<rust.SessionInfo>? sessions;

  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    final list = await widget.page.guard((c) => c.sessions());
    if (mounted && list != null) setState(() => sessions = list);
  }

  Future<void> show(String kind, String title, {BigInt? attach}) async {
    await Navigator.push(
      context,
      MaterialPageRoute(
        builder: (_) => TerminalPage(
          connection: widget.connection,
          kind: kind,
          title: title,
          attach: attach,
        ),
      ),
    );
    // Leaving detaches on its way out; look once it has arrived too.
    load();
    Future.delayed(const Duration(milliseconds: 500), load);
  }

  Future<void> kill(rust.SessionInfo session) async {
    final name = session.agent ? t.agent : '${t.shell} ${session.id}';
    if (!await confirm(context, t.killAsk(name), t.kill)) return;
    await widget.page.guard((c) => c.kill(session: session.id));
    load();
  }

  @override
  Widget build(BuildContext context) {
    final sessions = this.sessions;
    return RefreshIndicator(
      onRefresh: load,
      child: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          Wrap(
            spacing: 12,
            runSpacing: 12,
            children: [
              FilledButton.icon(
                icon: const Icon(Icons.smart_toy_outlined),
                label: Text(t.agent),
                onPressed: () => show('agent', t.agent),
              ),
              FilledButton.tonalIcon(
                icon: const Icon(Icons.terminal),
                label: Text(t.newShell),
                onPressed: () => show('shell', t.shell),
              ),
            ],
          ),
          const SizedBox(height: 16),
          if (sessions == null) const LinearProgressIndicator(),
          for (final session in sessions ?? <rust.SessionInfo>[])
            Card(
              child: ListTile(
                leading: Icon(
                  session.agent ? Icons.smart_toy_outlined : Icons.terminal,
                ),
                title: Text(
                  session.agent ? t.agent : '${t.shell} ${session.id}',
                ),
                subtitle: Text(
                  [
                    when(session.started),
                    session.running ? t.running : t.ended,
                    if (session.attached > 0) t.attached(session.attached),
                  ].join(' · '),
                ),
                onTap: session.running
                    ? () => show(
                        session.agent ? 'agent' : 'shell',
                        session.agent ? t.agent : '${t.shell} ${session.id}',
                        attach: session.id,
                      )
                    : null,
                trailing: session.running
                    ? IconButton(
                        tooltip: t.kill,
                        icon: const Icon(Icons.stop_circle_outlined),
                        onPressed: () => kill(session),
                      )
                    : null,
              ),
            ),
        ],
      ),
    );
  }
}

/// Text the device composes, such as its status or logs.
class _Text extends StatefulWidget {
  const _Text({required this.load, this.tail = false});

  final Future<String?> Function() load;

  /// Starts at the end, where the newest lines are.
  final bool tail;

  @override
  State<_Text> createState() => _TextState();
}

class _TextState extends State<_Text> {
  final scroll = ScrollController();
  String? text;

  @override
  void initState() {
    super.initState();
    load();
  }

  @override
  void dispose() {
    scroll.dispose();
    super.dispose();
  }

  Future<void> load() async {
    final loaded = await widget.load();
    if (!mounted || loaded == null) return;
    setState(() => text = loaded);
    if (widget.tail) {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (scroll.hasClients) scroll.jumpTo(scroll.position.maxScrollExtent);
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final text = this.text;
    if (text == null) return const Center(child: CircularProgressIndicator());
    return RefreshIndicator(
      onRefresh: load,
      child: SingleChildScrollView(
        controller: scroll,
        physics: const AlwaysScrollableScrollPhysics(),
        padding: const EdgeInsets.all(16),
        child: SelectableText(
          text,
          style: const TextStyle(fontFamily: 'monospace', fontSize: 13),
        ),
      ),
    );
  }
}

class _Files extends StatefulWidget {
  const _Files({required this.page});

  final _DevicePageState page;

  @override
  State<_Files> createState() => _FilesState();
}

class _FilesState extends State<_Files> {
  final t = T.of;
  final folder = TextEditingController(text: '/root');
  final path = TextEditingController();
  bool busy = false;
  String? done;

  @override
  void dispose() {
    folder.dispose();
    path.dispose();
    super.dispose();
  }

  Future<void> busyWith(Future<void> Function() work) async {
    setState(() {
      busy = true;
      done = null;
    });
    try {
      await work();
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  Future<void> upload() async {
    final picked = await FilePicker.pickFiles();
    if (picked.isEmpty) return;
    final file = picked.first;
    final dir = folder.text.trim().replaceAll(RegExp(r'/+$'), '');
    final remote = '$dir/${file.name}';
    await busyWith(() async {
      var local = file.path;
      File? copy;
      if (local == null) {
        // Not on disk (an Android content URI): copy it there first.
        final temp = await getTemporaryDirectory();
        copy = File('${temp.path}/${file.name}');
        final sink = copy.openWrite();
        await sink.addStream(file.readAsByteStream());
        await sink.close();
        local = copy.path;
      }
      try {
        final sent = await widget.page.guard(
          (c) => c.upload(local: local!, remote: remote),
        );
        if (sent != null && mounted) {
          setState(() => done = t.sent(sent.toInt(), remote));
        }
      } finally {
        await copy?.delete();
      }
    });
  }

  Future<void> download() async {
    final remote = path.text.trim();
    if (remote.isEmpty) return;
    final name = remote
        .split('/')
        .lastWhere((p) => p.isNotEmpty, orElse: () => 'file');
    if (Platform.isAndroid) {
      await busyWith(() async {
        final temp = await getTemporaryDirectory();
        final local = '${temp.path}/$name';
        final got = await widget.page.guard(
          (c) => c.download(remote: remote, local: local),
        );
        if (got == null) return;
        final file = File(local);
        final saved = await FilePicker.saveFile(
          fileName: name,
          bytes: await file.readAsBytes(),
        );
        await file.delete();
        if (saved != null && mounted) {
          setState(() => done = t.fetched(got.toInt(), name));
        }
      });
      return;
    }
    final dir =
        await FilePicker.getDirectoryPath() ??
        (await getDownloadsDirectory())?.path;
    if (dir == null) return;
    final local = '$dir${Platform.pathSeparator}$name';
    await busyWith(() async {
      final got = await widget.page.guard(
        (c) => c.download(remote: remote, local: local),
      );
      if (got != null && mounted) {
        setState(() => done = t.fetched(got.toInt(), local));
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    return ListView(
      padding: const EdgeInsets.all(16),
      children: [
        TextField(
          controller: folder,
          decoration: InputDecoration(
            labelText: t.remoteDir,
            border: const OutlineInputBorder(),
          ),
        ),
        const SizedBox(height: 8),
        Align(
          alignment: Alignment.centerLeft,
          child: FilledButton.tonalIcon(
            icon: const Icon(Icons.upload),
            label: Text(t.upload),
            onPressed: busy ? null : upload,
          ),
        ),
        const SizedBox(height: 24),
        TextField(
          controller: path,
          decoration: InputDecoration(
            labelText: t.remotePath,
            hintText: '/var/log/runod.log',
            border: const OutlineInputBorder(),
          ),
          onSubmitted: (_) => busy ? null : download(),
        ),
        const SizedBox(height: 8),
        Align(
          alignment: Alignment.centerLeft,
          child: FilledButton.tonalIcon(
            icon: const Icon(Icons.download),
            label: Text(t.download),
            onPressed: busy ? null : download,
          ),
        ),
        const SizedBox(height: 16),
        if (busy) const LinearProgressIndicator(),
        if (done != null) Text(done!),
      ],
    );
  }
}

class _Clients extends StatefulWidget {
  const _Clients({required this.page});

  final _DevicePageState page;

  @override
  State<_Clients> createState() => _ClientsState();
}

class _ClientsState extends State<_Clients> {
  final t = T.of;
  List<rust.ClientInfo>? clients;

  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    final list = await widget.page.guard((c) => c.clients());
    if (mounted && list != null) setState(() => clients = list);
  }

  Future<void> revoke(rust.ClientInfo client) async {
    if (!await confirm(context, t.revokeAsk(client.name), t.revoke)) return;
    await widget.page.guard((c) => c.revoke(client: client.id));
    load();
  }

  Future<void> openPairing() async {
    final opened = await widget.page.guard((c) async {
      await c.openPairing();
      return true;
    });
    if (opened == true && mounted) {
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text(t.pairingOpened)));
    }
  }

  @override
  Widget build(BuildContext context) {
    final clients = this.clients;
    return RefreshIndicator(
      onRefresh: load,
      child: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          Align(
            alignment: Alignment.centerLeft,
            child: FilledButton.tonalIcon(
              icon: const Icon(Icons.add_moderator_outlined),
              label: Text(t.openPairing),
              onPressed: openPairing,
            ),
          ),
          const SizedBox(height: 16),
          if (clients == null) const LinearProgressIndicator(),
          for (final client in clients ?? <rust.ClientInfo>[])
            Card(
              child: ListTile(
                leading: Icon(client.connected ? Icons.link : Icons.link_off),
                title: Text(
                  client.this_
                      ? '${client.name} (${t.thisClient})'
                      : client.name,
                ),
                subtitle: Text(
                  [
                    client.id,
                    client.connected
                        ? t.online
                        : t.lastSeen(
                            client.lastSeen == null
                                ? t.never
                                : when(client.lastSeen!),
                          ),
                  ].join(' · '),
                ),
                trailing: client.this_
                    ? null
                    : IconButton(
                        tooltip: t.revoke,
                        icon: const Icon(Icons.block),
                        onPressed: () => revoke(client),
                      ),
              ),
            ),
        ],
      ),
    );
  }
}
