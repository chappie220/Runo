import 'dart:ui';

/// The app's words, in Chinese when the system speaks it and English
/// otherwise; [lang] is what the device is asked to answer in.
class T {
  T._(this.zh);

  final bool zh;

  static final T of = T._(
    PlatformDispatcher.instance.locale.languageCode == 'zh',
  );

  String get lang => zh ? 'zh' : 'en';

  String t(String en, String zhText) => zh ? zhText : en;

  String get devices => t('Devices', '设备');
  String get paired => t('Paired', '已配对');
  String get nearby => t('On this network', '本网络中');
  String get searching => t('Looking on the network…', '正在本网络中查找…');
  String get noneNearby => t(
    'No other Runo device answered. Pairing needs the device on the same network.',
    '没有找到其他 Runo 设备。配对时设备需与本机在同一网络。',
  );
  String get noneYet => t('No device paired yet.', '还没有配对的设备。');
  String get refresh => t('Look again', '重新查找');
  String get addAddress => t('Pair by address', '按地址配对');
  String get address => t('Address (IP or IP:port)', '地址（IP 或 IP:端口）');
  String get pair => t('Pair', '配对');
  String get pairingOpen => t('Pairing open', '可配对');
  String get pairingClosed => t('Pairing closed on the device', '设备未开放配对');
  String get incompatible => t(
    'Speaks another protocol version; update the app or the device.',
    '协议版本不同，请更新应用或设备。',
  );
  String get forget => t('Forget', '忘记');
  String forgetAsk(String name) => t(
    'Forget $name? Pairing again needs the device password.',
    '忘记 $name？再次配对需要设备密码。',
  );
  String get cancel => t('Cancel', '取消');
  String get ok => t('OK', '确定');
  String get connecting => t('Connecting…', '正在连接…');
  String get compareCode => t(
    'Check that the device shows the same code (in the management view, or with `runo daemon pair`).',
    '请确认设备上显示相同的配对码（在管理界面中，或运行 `runo daemon pair`）。',
  );
  String get password => t('Device password', '设备密码');
  String get clientName => t('This client\'s name', '本客户端名称');
  String get codesMatch => t('Codes match, pair', '配对码一致，配对');
  String get alreadyTrusted =>
      t('The device already trusts this client.', '设备已信任本客户端。');
  String get pairedDone => t('Paired.', '已配对。');
  String get agent => t('Agent', 'Agent');
  String get shell => t('Shell', 'Shell');
  String get sessions => t('Sessions', '会话');
  String get status => t('Status', '状态');
  String get logs => t('Logs', '日志');
  String get files => t('Files', '文件');
  String get clients => t('Clients', '客户端');
  String get newShell => t('New shell', '新建 Shell');
  String get attach => t('Show', '查看');
  String get kill => t('End', '结束');
  String killAsk(String what) =>
      t('End $what? What runs in it stops.', '结束$what？其中运行的程序会停止。');
  String get running => t('running', '运行中');
  String get ended => t('ended', '已结束');
  String attached(int n) => t('$n showing', '$n 处查看');
  String sessionEnded(int? code) => code == null
      ? t('Session ended.', '会话已结束。')
      : t('Session ended ($code).', '会话已结束（$code）。');
  String get detach => t('Leave running', '保持运行并离开');
  String get closed => t('Disconnected', '已断开');
  String get reconnect => t('Reconnect', '重新连接');
  String get upload => t('Send a file', '发送文件');
  String get download => t('Fetch a file', '获取文件');
  String get remotePath => t('Path on the device', '设备上的路径');
  String get remoteDir => t('Folder on the device', '设备上的目录');
  String sent(int n, String to) =>
      t('Sent $n bytes to $to.', '已发送 $n 字节到 $to。');
  String fetched(int n, String to) =>
      t('Fetched $n bytes to $to.', '已获取 $n 字节到 $to。');
  String get openPairing => t('Let another client pair', '允许另一客户端配对');
  String get pairingOpened => t(
    'Pairing is open for 5 minutes. Pair the other client on this network.',
    '配对已开放 5 分钟。请在本网络中配对另一客户端。',
  );
  String get revoke => t('Revoke', '撤销');
  String revokeAsk(String name) => t(
    'Revoke $name? It can no longer reach the device.',
    '撤销 $name？它将无法再连接设备。',
  );
  String get thisClient => t('this client', '本客户端');
  String get online => t('connected', '已连接');
  String lastSeen(String when) => t('last seen $when', '最近 $when');
  String get never => t('never', '从未');
  String get keys => t('Keys', '按键');
  String waiting(int id, String summary, int seconds) => t(
    'Change #$id ($summary) may have cut this client off. Keep it? It is put back in $seconds s.',
    '修改 #$id（$summary）可能导致本客户端失联。保留吗？$seconds 秒后自动撤回。',
  );
  String get keep => t('Keep', '保留');
  String get putBack => t('Put back', '撤回');
  String get kept => t('Change kept.', '已保留修改。');
  String get putBackDone => t('Change put back.', '已撤回修改。');
}
