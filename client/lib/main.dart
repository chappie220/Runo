import 'package:flutter/material.dart';

import 'devices.dart';
import 'src/rust/frb_generated.dart';
import 'store.dart';
import 'text.dart';

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
    const seed = Color(0xFF2E7D6B);
    return MaterialApp(
      title: 'Runo',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(colorSchemeSeed: seed, useMaterial3: true),
      darkTheme: ThemeData(
        colorSchemeSeed: seed,
        brightness: Brightness.dark,
        useMaterial3: true,
      ),
      home: DevicesPage(store: store),
    );
  }
}

/// Shows `error` at the bottom of the screen.
void complain(BuildContext context, Object error) {
  ScaffoldMessenger.of(context)
      .showSnackBar(SnackBar(content: Text(message(error))));
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
      content: Text(question),
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
