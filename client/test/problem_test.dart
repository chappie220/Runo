import 'package:flutter_rust_bridge/flutter_rust_bridge.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:runo_app/problem.dart';

void main() {
  test('drops the stack backtrace and keeps the causes as details', () {
    final problem = Problem.of(
      AnyhowException(
        'could not open the connection\n'
        '\n'
        'Caused by:\n'
        '    0: dialing 192.168.1.20:7340\n'
        '    1: Connection timed out (os error 110)\n'
        '\n'
        'Stack backtrace:\n'
        '   0: anyhow::error::<impl anyhow::Error>::msg\n'
        '   1: runo_client::api::client::Connection::open\n',
      ),
    );
    expect(problem.title, 'Could not reach the device.');
    expect(
      problem.details,
      'could not open the connection\n'
      'dialing 192.168.1.20:7340\n'
      'Connection timed out (os error 110)',
    );
  });

  test('puts what the device said in words', () {
    final problem = Problem.of(
      AnyhowException(
        'the device refused: wrong device password\n\n'
        'Stack backtrace:\n   0: x\n',
      ),
    );
    expect(problem.title, 'The device password is wrong.');
    expect(problem.details, 'the device refused: wrong device password');
  });

  test('keeps an unknown one-line error as it is', () {
    final problem = Problem.of(AnyhowException('no session 4'));
    expect(problem.title, 'no session 4');
    expect(problem.details, isNull);
  });
}
