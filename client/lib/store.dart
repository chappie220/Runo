import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:path_provider/path_provider.dart';

import 'problem.dart';
import 'src/rust/api/client.dart' as rust;

/// Where the app keeps what it knows: the client's identity key in the
/// system's secure storage, the paired devices in its support folder.
class Store {
  Store._(this.dir, this.identity) {
    look.value = switch (_read('look')) {
      'light' => ThemeMode.light,
      'dark' => ThemeMode.dark,
      _ => ThemeMode.system,
    };
    look.addListener(() => _write('look', look.value.name));
  }

  /// The folder for the paired devices (`devices.json`).
  final String dir;

  /// This client's identity, as PEM.
  final String identity;

  String get id => rust.identityId(pem: identity);

  /// Light, dark, or as the system is; kept in `look` beside the devices.
  final look = ValueNotifier(ThemeMode.system);

  String? _read(String name) {
    try {
      return File('$dir${Platform.pathSeparator}$name')
          .readAsStringSync()
          .trim();
    } on FileSystemException {
      return null;
    }
  }

  void _write(String name, String value) {
    try {
      File('$dir${Platform.pathSeparator}$name').writeAsStringSync(value);
    } on FileSystemException {
      // Not kept: this run still has it.
    }
  }

  static const _key = 'runo.identity';

  static Future<Store> open() async {
    final support = await getApplicationSupportDirectory();
    final dir = Directory('${support.path}${Platform.pathSeparator}runo');
    await dir.create(recursive: true);
    const storage = FlutterSecureStorage();
    String identity;
    try {
      identity = await storage.read(key: _key) ?? rust.newIdentity();
      await storage.write(key: _key, value: identity);
    } on PlatformException {
      // No secure store (a Linux desktop without a keyring): a file only
      // this user can read, as `runo remote` keeps it.
      identity = rust.fileIdentity(dir: dir.path);
    }
    return Store._(dir.path, identity);
  }
}

/// Lets mDNS answers in on Android, which drops multicast unless an app
/// holds a lock while it looks.
class Multicast {
  static const _channel = MethodChannel('dev.runo/multicast');

  static Future<T> hold<T>(Future<T> Function() look) async {
    if (!Platform.isAndroid) return look();
    try {
      await _channel.invokeMethod('acquire');
    } on PlatformException {
      // Looking still works on networks that pass multicast anyway.
    }
    try {
      return await look();
    } finally {
      try {
        await _channel.invokeMethod('release');
      } on PlatformException {
        // Nothing held.
      }
    }
  }
}

/// A short local date and time for a Unix timestamp.
String when(int seconds) {
  final t = DateTime.fromMillisecondsSinceEpoch(seconds * 1000);
  String two(int n) => n.toString().padLeft(2, '0');
  return '${t.year}-${two(t.month)}-${two(t.day)} ${two(t.hour)}:${two(t.minute)}';
}

/// The message of an error from the device or the bridge, in one line.
String message(Object error) => Problem.of(error).title;
