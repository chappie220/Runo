import 'package:flutter_rust_bridge/flutter_rust_bridge.dart';

import 'text.dart';

/// An error put in words a person reads: one short [title], and the
/// device's or bridge's own text in [details] for whoever wants it.
class Problem {
  const Problem(this.title, [this.details]);

  final String title;
  final String? details;

  /// Reads an error from the device or the bridge. The bridge hands over
  /// anyhow's whole report, causes and stack backtrace included; only the
  /// causes are kept, and only behind [details].
  factory Problem.of(Object error) {
    var text = error is AnyhowException ? error.message : error.toString();
    final backtrace = text.indexOf('Stack backtrace:');
    if (backtrace >= 0) text = text.substring(0, backtrace);
    text = text.trim();
    final lines = text
        .split('\n')
        .map((line) => line.trim())
        .where((line) => line.isNotEmpty && line != 'Caused by:')
        .map((line) => line.replaceFirst(RegExp(r'^\d+: '), ''))
        .toList();
    final headline = lines.isEmpty ? text : lines.first;
    final known = _known(lines.join('\n').toLowerCase());
    final details = lines.join('\n');
    if (known != null) return Problem(known, details);
    return Problem(headline, lines.length > 1 ? details : null);
  }

  @override
  String toString() => title;
}

String? _known(String text) {
  final t = T.of;
  bool has(String part) => text.contains(part);
  if (has('too many tries')) {
    return t.t(
      'Wrong device password too many times, so the device closed pairing. Open pairing on the device again.',
      '设备密码错误次数太多，设备已关闭配对窗口。请在设备上重新开放配对。',
    );
  }
  if (has('locked out')) {
    return t.t(
      'Too many wrong passwords: the device is not taking more for a while.',
      '密码错误次数太多，设备暂时不再接受尝试。',
    );
  }
  if (has('wrong device password')) {
    return t.t('The device password is wrong.', '设备密码不对。');
  }
  if (has('pairing window is closed')) {
    return t.t(
      'Pairing is closed on the device. Open it there (Pairing in the management view, or runo daemon pair), then try again.',
      '设备没有开放配对。请先在设备上开放（管理界面中的“配对”，或运行 runo daemon pair），再重试。',
    );
  }
  if (has('pairing works only on the local network')) {
    return t.t(
      'Pairing works only on the device\'s own network.',
      '只能在设备所在的局域网里配对。',
    );
  }
  if (has('no longer trusts this client') ||
      has('not paired with the device')) {
    return t.t(
      'The device no longer trusts this client: forget it and pair again.',
      '设备已不再信任本客户端：请忘记该设备后重新配对。',
    );
  }
  if (has('no device password')) {
    return t.t(
      'The device has no password yet, so it cannot be reached from outside its network.',
      '设备还没有设置密码，因此无法从它所在网络之外连接。',
    );
  }
  if (has('connection refused')) {
    return t.t(
      'The device is there but Runo is not answering on it.',
      '找到了设备，但上面的 Runo 没有响应。',
    );
  }
  if (has('timed out') ||
      has('handshake') ||
      has('connection reset') ||
      has('unreachable') ||
      has('no route to host') ||
      has('failed to lookup') ||
      has('no address to connect to')) {
    return t.t('Could not reach the device.', '连不上设备。');
  }
  return null;
}
