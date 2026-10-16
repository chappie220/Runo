import 'dart:io';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';

import 'agent.dart';
import 'changes.dart';
import 'feed.dart';
import 'main.dart';
import 'problem.dart';
import 'src/rust/api/client.dart' as rust;
import 'store.dart';
import 'terminal.dart';
import 'text.dart';
import 'theme.dart';
import 'widgets.dart';

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
  Problem? error;

  /// What to try about [error].
  String? hint;

  /// Reached from outside its network, through its domain.
  bool away = false;
  int generation = 0;
  int tab = 0;

  /// Counts the times the user asked for the page again.
  int reloads = 0;

  @override
  void initState() {
    super.initState();
    connect();
  }

  Future<void> connect() async {
    setState(() {
      connection = null;
      error = null;
      hint = null;
    });
    try {
      final opened = await Multicast.hold(
        () => rust.Connection.open(
          dir: widget.store.dir,
          identity: widget.store.identity,
          id: widget.device.id,
        ),
      );
      if (opened.needsLogin() && !await logIn(opened)) {
        if (mounted) setState(() => error = Problem(t.loginNeeded));
        return;
      }
      if (mounted) {
        setState(() {
          connection = opened;
          away = opened.needsLogin();
          generation++;
        });
      }
    } catch (e) {
      // From outside its network the device is reached only through its
      // domain, which needs IPv6 and the router's leave.
      final domain = widget.device.domain;
      if (mounted) {
        setState(() {
          error = Problem.of(e);
          hint = domain == null ? t.outsideNoDomain : t.outsideDomain(domain);
        });
      }
    }
  }

  /// Asks for the device password until it is right or the user gives up;
  /// the device asks for it when reached from outside its network.
  Future<bool> logIn(rust.Connection opened) async {
    String? wrong;
    while (mounted) {
      final password = await askPassword(wrong);
      if (password == null) return false;
      try {
        await opened.login(password: password);
        return true;
      } catch (e) {
        wrong = message(e);
      }
    }
    return false;
  }

  Future<String?> askPassword(String? wrong) {
    final field = TextEditingController();
    return showDialog<String>(
      context: context,
      barrierDismissible: false,
      builder: (context) => AlertDialog(
        title: Text(t.logIn),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(t.loginAbout(widget.device.name)),
            const SizedBox(height: 12),
            TextField(
              controller: field,
              obscureText: true,
              autofocus: true,
              decoration: InputDecoration(
                labelText: t.password,
                errorText: wrong,
              ),
              onSubmitted: (value) => Navigator.pop(context, value),
            ),
          ],
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: Text(t.cancel),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, field.text),
            child: Text(t.logIn),
          ),
        ],
      ),
    );
  }

  /// The connection, or a new one when it was lost; none when the device
  /// cannot be reached.
  Future<rust.Connection?> live() async {
    final connection = this.connection;
    if (connection != null && connection.closed() == null) return connection;
    await connect();
    return this.connection;
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
        setState(() => error = Problem(t.closed, closed));
      } else {
        complain(context, e);
      }
      return null;
    }
  }

  @override
  Widget build(BuildContext context) {
    final connection = this.connection;
    final colors = Theme.of(context).colorScheme;
    final tabs = [
      (Icons.terminal, t.sessions),
      (Icons.info_outline, t.status),
      (Icons.receipt_long_outlined, t.logs),
      (Icons.folder_outlined, t.files),
      (Icons.devices_outlined, t.clients),
    ];
    final wide = MediaQuery.sizeOf(context).width >= 840;
    final Widget body;
    if (error != null) {
      body = ProblemView(
        problem: error!,
        hint: hint,
        retry: connect,
        retryLabel: t.reconnect,
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
      final Widget page = switch (tab) {
        0 => _Sessions(page: this, connection: connection),
        1 => _Text(load: () => guard((c) => c.status(lang: t.lang))),
        2 => _Text(load: () => guard((c) => c.logs(lang: t.lang)), tail: true),
        3 => _Files(page: this),
        _ => _Clients(page: this),
      };
      final content = ChangeBanner(
        key: ValueKey(generation),
        connection: connection,
        child: KeyedSubtree(key: ValueKey((tab, reloads)), child: page),
      );
      body = wide
          ? Row(
              children: [
                NavigationRail(
                  selectedIndex: tab,
                  onDestinationSelected: (i) => setState(() => tab = i),
                  destinations: [
                    for (final (icon, label) in tabs)
                      NavigationRailDestination(
                        icon: Icon(icon),
                        label: Text(label),
                      ),
                  ],
                ),
                VerticalDivider(width: 1, color: colors.outlineVariant),
                Expanded(child: content),
              ],
            )
          : content;
    }
    return Scaffold(
      appBar: AppBar(
        title: Text(connection?.deviceName ?? widget.device.name),
        actions: [
          if (connection != null) ...[
            Pill(
              away ? t.onRemote : t.onLan,
              color: good(context),
              icon: away ? Icons.public : Icons.lan_outlined,
            ),
            IconButton(
              tooltip: t.reload,
              icon: const Icon(Icons.refresh),
              onPressed: () => setState(() => reloads++),
            ),
          ],
          const SizedBox(width: 8),
        ],
      ),
      body: body,
      bottomNavigationBar: connection == null || wide
          ? null
          : NavigationBar(
              selectedIndex: tab,
              onDestinationSelected: (i) => setState(() => tab = i),
              labelBehavior: NavigationDestinationLabelBehavior.alwaysShow,
              destinations: [
                for (final (icon, label) in tabs)
                  NavigationDestination(icon: Icon(icon), label: label),
              ],
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
  bool showEnded = false;

  /// The number each running shell goes by here. The device counts every
  /// session it ever started; this takes the smallest number free, so a
  /// shell opened after the others closed is "Shell 1" again.
  final numbers = <BigInt, int>{};

  int get free {
    var n = 1;
    while (numbers.containsValue(n)) {
      n++;
    }
    return n;
  }

  void count(List<rust.SessionInfo> list) {
    final shells = list.where((s) => s.running && !s.agent).toList()
      ..sort((a, b) => a.id.compareTo(b.id));
    numbers.removeWhere((id, _) => !shells.any((s) => s.id == id));
    for (final shell in shells) {
      numbers.putIfAbsent(shell.id, () => free);
    }
  }

  String name(rust.SessionInfo session) => session.agent
      ? t.agent
      : numbers[session.id] == null
      ? t.shell
      : '${t.shell} ${numbers[session.id]}';

  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    final list = await widget.page.guard((c) => c.sessions());
    if (mounted && list != null) {
      setState(() {
        count(list);
        sessions = list;
      });
    }
  }

  /// A terminal, or the Agent as its event stream where the device has
  /// one (`feed`: a feed session, or a new Agent).
  Future<void> show(
    String kind,
    String title, {
    BigInt? attach,
    bool feed = false,
  }) async {
    await Navigator.push(
      context,
      MaterialPageRoute(
        builder: (_) => feed
            ? AgentPage(
                open: rustFeed(widget.page.live, attach: attach),
                title: title,
                connection: widget.connection,
              )
            : TerminalPage(
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
    final name = this.name(session);
    if (!await confirm(context, t.killAsk(name), t.kill)) return;
    await widget.page.guard((c) => c.kill(session: session.id));
    load();
  }

  Widget card(rust.SessionInfo session) {
    final theme = Theme.of(context);
    return Card(
      child: InkWell(
        onTap: session.running
            ? () => show(
                session.agent ? 'agent' : 'shell',
                name(session),
                attach: session.id,
                feed: session.feed,
              )
            : null,
        child: Padding(
          padding: const EdgeInsets.fromLTRB(16, 14, 8, 14),
          child: Row(
            children: [
              Tile(
                session.agent ? Icons.smart_toy_outlined : Icons.terminal,
                active: session.running,
              ),
              const SizedBox(width: 14),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      name(session),
                      style: theme.textTheme.titleMedium?.copyWith(
                        fontWeight: FontWeight.w600,
                      ),
                    ),
                    Text(
                      when(session.started),
                      style: theme.textTheme.bodySmall?.copyWith(
                        color: theme.colorScheme.onSurfaceVariant,
                      ),
                    ),
                    const SizedBox(height: 8),
                    Wrap(
                      spacing: 6,
                      runSpacing: 6,
                      children: [
                        Pill(
                          session.running ? t.running : t.ended,
                          color: state(context, session.running),
                        ),
                        if (session.attached > 0)
                          Pill(
                            t.attached(session.attached),
                            color: theme.colorScheme.primary,
                            icon: Icons.visibility_outlined,
                          ),
                      ],
                    ),
                  ],
                ),
              ),
              if (session.running)
                IconButton(
                  tooltip: t.kill,
                  icon: const Icon(Icons.stop_circle_outlined),
                  onPressed: () => kill(session),
                ),
            ],
          ),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final sessions = this.sessions;
    // An ended session stays listed on the device for a while; it is
    // kept out of the way here.
    final running = [...?sessions?.where((s) => s.running)];
    final ended = [...?sessions?.where((s) => !s.running)];
    return Listing(
      onRefresh: load,
      children: [
        Wrap(
          spacing: 12,
          runSpacing: 12,
          children: [
            FilledButton.icon(
              icon: const Icon(Icons.smart_toy_outlined),
              label: Text(t.agent),
              onPressed: () =>
                  show('agent', t.agent, feed: widget.connection.hasFeed()),
            ),
            FilledButton.tonalIcon(
              icon: const Icon(Icons.add),
              label: Text(t.newShell),
              onPressed: () => show('shell', '${t.shell} $free'),
            ),
          ],
        ),
        Heading(t.sessions),
        if (sessions == null)
          const LinearProgressIndicator()
        else if (running.isEmpty)
          Empty(icon: Icons.terminal, text: t.noSessions)
        else
          Cards(children: [for (final session in running) card(session)]),
        if (ended.isNotEmpty) ...[
          const SizedBox(height: 16),
          Align(
            alignment: Alignment.centerLeft,
            child: TextButton.icon(
              icon: Icon(showEnded ? Icons.expand_less : Icons.expand_more),
              label: Text(t.endedSessions(ended.length)),
              onPressed: () => setState(() => showEnded = !showEnded),
            ),
          ),
          if (showEnded) ...[
            const SizedBox(height: 8),
            Cards(children: [for (final session in ended) card(session)]),
          ],
        ],
      ],
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
      child: LayoutBuilder(
        builder: (context, box) {
          final side = box.maxWidth > 1112 ? (box.maxWidth - 1080) / 2 : 16.0;
          return ListView(
            controller: scroll,
            physics: const AlwaysScrollableScrollPhysics(),
            padding: EdgeInsets.fromLTRB(side, 16, side, 24),
            children: [
              Panel(
                child: SelectableText(
                  text,
                  style: mono.copyWith(fontSize: 13, height: 1.45),
                ),
              ),
            ],
          );
        },
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
    final theme = Theme.of(context);
    Widget card({
      required IconData icon,
      required String title,
      required String about,
      required Widget field,
      required Widget button,
    }) => Card(
      child: Padding(
        padding: const EdgeInsets.all(20),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Tile(icon),
                const SizedBox(width: 14),
                Expanded(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        title,
                        style: theme.textTheme.titleMedium?.copyWith(
                          fontWeight: FontWeight.w600,
                        ),
                      ),
                      Text(
                        about,
                        style: theme.textTheme.bodySmall?.copyWith(
                          color: theme.colorScheme.onSurfaceVariant,
                        ),
                      ),
                    ],
                  ),
                ),
              ],
            ),
            const SizedBox(height: 20),
            field,
            const SizedBox(height: 12),
            Align(alignment: Alignment.centerRight, child: button),
          ],
        ),
      ),
    );
    return Listing(
      children: [
        Cards(
          children: [
            card(
              icon: Icons.upload,
              title: t.upload,
              about: t.sendAbout,
              field: TextField(
                controller: folder,
                style: mono,
                decoration: InputDecoration(
                  labelText: t.remoteDir,
                  prefixIcon: const Icon(Icons.folder_outlined),
                ),
              ),
              button: FilledButton.tonalIcon(
                icon: const Icon(Icons.file_open_outlined),
                label: Text(t.upload),
                onPressed: busy ? null : upload,
              ),
            ),
            card(
              icon: Icons.download,
              title: t.download,
              about: t.fetchAbout,
              field: TextField(
                controller: path,
                style: mono,
                decoration: InputDecoration(
                  labelText: t.remotePath,
                  hintText: '/var/log/runod.log',
                  prefixIcon: const Icon(Icons.description_outlined),
                ),
                onSubmitted: (_) => busy ? null : download(),
              ),
              button: FilledButton.tonalIcon(
                icon: const Icon(Icons.download),
                label: Text(t.download),
                onPressed: busy ? null : download,
              ),
            ),
          ],
        ),
        const SizedBox(height: 16),
        if (busy) const LinearProgressIndicator(),
        if (done != null)
          Card(
            color: good(context).withValues(alpha: .1),
            child: ListTile(
              leading: Icon(Icons.check_circle, color: good(context)),
              title: Text(done!),
            ),
          ),
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
    final theme = Theme.of(context);
    return Listing(
      onRefresh: load,
      children: [
        Card(
          color: theme.colorScheme.secondaryContainer.withValues(alpha: .5),
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Wrap(
              spacing: 16,
              runSpacing: 12,
              crossAxisAlignment: WrapCrossAlignment.center,
              alignment: WrapAlignment.spaceBetween,
              children: [
                Text(t.clientsAbout, style: theme.textTheme.bodyMedium),
                FilledButton.tonalIcon(
                  icon: const Icon(Icons.add_moderator_outlined),
                  label: Text(t.openPairing),
                  onPressed: openPairing,
                ),
              ],
            ),
          ),
        ),
        Heading(t.clients),
        if (clients == null) const LinearProgressIndicator(),
        Cards(
          children: [
            for (final client in clients ?? <rust.ClientInfo>[])
              Card(
                child: Padding(
                  padding: const EdgeInsets.fromLTRB(16, 14, 8, 14),
                  child: Row(
                    children: [
                      Tile(Icons.laptop, active: client.connected),
                      const SizedBox(width: 14),
                      Expanded(
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          children: [
                            Text(
                              client.name,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: theme.textTheme.titleMedium?.copyWith(
                                fontWeight: FontWeight.w600,
                              ),
                            ),
                            Text(
                              client.id,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: mono.copyWith(
                                fontSize: 12,
                                color: theme.colorScheme.onSurfaceVariant,
                              ),
                            ),
                            const SizedBox(height: 8),
                            Wrap(
                              spacing: 6,
                              runSpacing: 6,
                              children: [
                                Pill(
                                  client.connected
                                      ? t.online
                                      : t.lastSeen(
                                          client.lastSeen == null
                                              ? t.never
                                              : when(client.lastSeen!),
                                        ),
                                  color: state(context, client.connected),
                                ),
                                if (client.this_)
                                  Pill(
                                    t.thisClient,
                                    color: theme.colorScheme.primary,
                                    icon: Icons.person_outline,
                                  ),
                              ],
                            ),
                          ],
                        ),
                      ),
                      if (!client.this_)
                        IconButton(
                          tooltip: t.revoke,
                          icon: const Icon(Icons.block),
                          onPressed: () => revoke(client),
                        ),
                    ],
                  ),
                ),
              ),
          ],
        ),
      ],
    );
  }
}
