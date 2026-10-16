import 'package:flutter/material.dart';

import 'devices.dart';
import 'src/rust/frb_generated.dart';
import 'store.dart';
import 'text.dart';
import 'theme.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  final store = await Store.open();
  runApp(RunoApp(store: store));
}

class RunoApp extends StatelessWidget {
  const RunoApp({super.key, required this.store});

  final Store store;

  @override
  Widget build(BuildContext context) {
    return ValueListenableBuilder(
      valueListenable: store.look,
      builder: (context, look, home) => MaterialApp(
        title: 'Runo',
        debugShowCheckedModeBanner: false,
        theme: theme(Brightness.light),
        darkTheme: theme(Brightness.dark),
        themeMode: look,
        home: home,
      ),
      child: DevicesPage(store: store),
    );
  }
}

/// Shows `error` at the bottom of the screen.
void complain(BuildContext context, Object error) {
  final colors = Theme.of(context).colorScheme;
  ScaffoldMessenger.of(context).showSnackBar(
    SnackBar(
      content: Row(
        children: [
          Icon(Icons.error_outline, color: colors.errorContainer),
          const SizedBox(width: 12),
          Expanded(child: Text(message(error))),
        ],
      ),
    ),
  );
}

/// Asks before something that cannot be taken back.
Future<bool> confirm(
  BuildContext context,
  String question,
  String action,
) async {
  final t = T.of;
  final yes = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      icon: const Icon(Icons.warning_amber_rounded),
      content: Text(question, textAlign: TextAlign.center),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context, false),
          child: Text(t.cancel),
        ),
        FilledButton(
          onPressed: () => Navigator.pop(context, true),
          child: Text(action),
        ),
      ],
    ),
  );
  return yes ?? false;
}
