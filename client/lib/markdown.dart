import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'text.dart';
import 'theme.dart';

/// The Markdown the Agent writes, drawn with Flutter's own text: headings,
/// lists, quotes, tables, rules, fenced code with a copy button, and
/// inline code, bold, italics and links. A reply still arriving may end
/// in the middle of a block; an open code fence runs to the end.
class Markdown extends StatelessWidget {
  const Markdown(this.text, {super.key});

  final String text;

  @override
  Widget build(BuildContext context) {
    final blocks = parse(text);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final (i, block) in blocks.indexed)
          Padding(
            padding: EdgeInsets.only(top: i == 0 ? 0 : 8),
            child: _draw(context, block),
          ),
      ],
    );
  }

  Widget _draw(BuildContext context, MdBlock block) {
    final theme = Theme.of(context);
    final body = theme.textTheme.bodyLarge!.copyWith(height: 1.5);
    switch (block) {
      case MdCode(:final code, :final language):
        return CodeBlock(code: code, language: language);
      case MdHeading(:final level, :final text):
        final style = switch (level) {
          1 => theme.textTheme.titleLarge,
          2 => theme.textTheme.titleMedium,
          _ => theme.textTheme.titleSmall,
        };
        return Text.rich(
          inline(context, text, style!.copyWith(fontWeight: FontWeight.w700)),
        );
      case MdItem(:final depth, :final marker, :final text):
        return Padding(
          padding: EdgeInsets.only(left: 4.0 + depth * 18),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              SizedBox(
                width: marker == null ? 16 : 26,
                child: Text(marker ?? '•', style: body),
              ),
              Expanded(child: Text.rich(inline(context, text, body))),
            ],
          ),
        );
      case MdQuote(:final text):
        return Container(
          padding: const EdgeInsets.only(left: 12),
          decoration: BoxDecoration(
            border: Border(
              left: BorderSide(
                color: theme.colorScheme.outlineVariant,
                width: 3,
              ),
            ),
          ),
          child: Text.rich(
            inline(
              context,
              text,
              body.copyWith(color: theme.colorScheme.onSurfaceVariant),
            ),
          ),
        );
      case MdRule():
        return const Divider(height: 8);
      case MdTable(:final rows):
        final columns = rows.fold(0, (n, r) => r.length > n ? r.length : n);
        return SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: Table(
            defaultColumnWidth: const IntrinsicColumnWidth(),
            border: TableBorder.all(color: theme.colorScheme.outlineVariant),
            children: [
              for (final (i, row) in rows.indexed)
                TableRow(
                  decoration: i == 0
                      ? BoxDecoration(
                          color: theme.colorScheme.surfaceContainerHigh,
                        )
                      : null,
                  children: [
                    for (var c = 0; c < columns; c++)
                      Padding(
                        padding: const EdgeInsets.symmetric(
                          horizontal: 10,
                          vertical: 6,
                        ),
                        child: Text.rich(
                          inline(
                            context,
                            c < row.length ? row[c] : '',
                            i == 0
                                ? body.copyWith(fontWeight: FontWeight.w600)
                                : body,
                          ),
                        ),
                      ),
                  ],
                ),
            ],
          ),
        );
      case MdParagraph(:final text):
        return Text.rich(inline(context, text, body));
    }
  }
}

/// A fenced code block, with a button that copies it.
class CodeBlock extends StatelessWidget {
  const CodeBlock({super.key, required this.code, this.language});

  final String code;
  final String? language;

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    return Container(
      decoration: BoxDecoration(
        color: colors.surfaceContainerHighest,
        borderRadius: BorderRadius.circular(12),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              const SizedBox(width: 12),
              Expanded(
                child: Text(
                  language ?? '',
                  style: Theme.of(context).textTheme.labelSmall
                      ?.copyWith(color: colors.onSurfaceVariant),
                ),
              ),
              IconButton(
                tooltip: T.of.copy,
                iconSize: 18,
                visualDensity: VisualDensity.compact,
                icon: const Icon(Icons.copy),
                onPressed: () async {
                  await Clipboard.setData(ClipboardData(text: code));
                  if (context.mounted) {
                    ScaffoldMessenger.of(context).showSnackBar(
                      SnackBar(
                        content: Text(T.of.copied),
                        duration: const Duration(seconds: 1),
                      ),
                    );
                  }
                },
              ),
            ],
          ),
          SingleChildScrollView(
            scrollDirection: Axis.horizontal,
            padding: const EdgeInsets.fromLTRB(12, 0, 12, 12),
            child: Text(code, style: mono.copyWith(fontSize: 13, height: 1.45)),
          ),
        ],
      ),
    );
  }
}

sealed class MdBlock {}

class MdParagraph extends MdBlock {
  MdParagraph(this.text);
  final String text;
}

class MdHeading extends MdBlock {
  MdHeading(this.level, this.text);
  final int level;
  final String text;
}

class MdCode extends MdBlock {
  MdCode(this.code, this.language);
  final String code;
  final String? language;
}

class MdItem extends MdBlock {
  MdItem(this.depth, this.marker, this.text);
  final int depth;

  /// "3." for a numbered item, none for a bullet.
  final String? marker;
  final String text;
}

class MdQuote extends MdBlock {
  MdQuote(this.text);
  final String text;
}

class MdRule extends MdBlock {}

class MdTable extends MdBlock {
  MdTable(this.rows);
  final List<List<String>> rows;
}

final _fence = RegExp(r'^\s*(```|~~~)\s*([^\s`]*)');
final _heading = RegExp(r'^(#{1,6})\s+(.*?)\s*#*\s*$');
final _bullet = RegExp(r'^(\s*)[-*+]\s+(.*)$');
final _numbered = RegExp(r'^(\s*)(\d{1,9}[.)])\s+(.*)$');
final _rule = RegExp(r'^\s*([-*_])(\s*\1){2,}\s*$');
final _tableRule = RegExp(r'^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$');

/// [text] as blocks. Lines in a paragraph keep their breaks, as the Agent
/// means them.
List<MdBlock> parse(String text) {
  final blocks = <MdBlock>[];
  final lines = text.replaceAll('\r\n', '\n').split('\n');
  final paragraph = <String>[];
  void flush() {
    if (paragraph.isNotEmpty) blocks.add(MdParagraph(paragraph.join('\n')));
    paragraph.clear();
  }

  for (var i = 0; i < lines.length; i++) {
    final line = lines[i];
    final fence = _fence.firstMatch(line);
    if (fence != null) {
      flush();
      final close = fence[1]!;
      final code = <String>[];
      i++;
      while (i < lines.length && !lines[i].trimLeft().startsWith(close)) {
        code.add(lines[i]);
        i++;
      }
      blocks.add(MdCode(code.join('\n'), fence[2]!.isEmpty ? null : fence[2]));
      continue;
    }
    if (line.trim().isEmpty) {
      flush();
      continue;
    }
    if (_rule.hasMatch(line)) {
      flush();
      blocks.add(MdRule());
      continue;
    }
    final heading = _heading.firstMatch(line);
    if (heading != null) {
      flush();
      blocks.add(MdHeading(heading[1]!.length, heading[2]!));
      continue;
    }
    final bullet = _bullet.firstMatch(line);
    if (bullet != null) {
      flush();
      blocks.add(MdItem(bullet[1]!.length ~/ 2, null, bullet[2]!));
      continue;
    }
    final numbered = _numbered.firstMatch(line);
    if (numbered != null) {
      flush();
      blocks.add(MdItem(numbered[1]!.length ~/ 2, numbered[2]!, numbered[3]!));
      continue;
    }
    if (line.trimLeft().startsWith('>')) {
      flush();
      final quote = <String>[];
      while (i < lines.length && lines[i].trimLeft().startsWith('>')) {
        quote.add(lines[i].trimLeft().substring(1).trimLeft());
        i++;
      }
      i--;
      blocks.add(MdQuote(quote.join('\n')));
      continue;
    }
    if (line.trimLeft().startsWith('|') &&
        i + 1 < lines.length &&
        _tableRule.hasMatch(lines[i + 1])) {
      flush();
      final rows = [_cells(line)];
      i += 2;
      while (i < lines.length && lines[i].trimLeft().startsWith('|')) {
        rows.add(_cells(lines[i]));
        i++;
      }
      i--;
      blocks.add(MdTable(rows));
      continue;
    }
    paragraph.add(line);
  }
  flush();
  return blocks;
}

List<String> _cells(String line) {
  var row = line.trim();
  if (row.startsWith('|')) row = row.substring(1);
  if (row.endsWith('|')) row = row.substring(0, row.length - 1);
  return [for (final cell in row.split('|')) cell.trim()];
}

final _inline = RegExp(
  r'`([^`]+)`|\*\*(.+?)\*\*|__(.+?)__|\*([^*\s][^*]*?)\*|(?<![\w])_([^_\s][^_]*?)_(?![\w])|\[([^\]]+)\]\(([^)\s]+)\)',
);

/// One line's inline Markdown as spans.
TextSpan inline(BuildContext context, String text, TextStyle style) {
  final colors = Theme.of(context).colorScheme;
  final spans = <InlineSpan>[];
  var at = 0;
  for (final m in _inline.allMatches(text)) {
    if (m.start > at) spans.add(TextSpan(text: text.substring(at, m.start)));
    if (m[1] != null) {
      spans.add(
        TextSpan(
          text: m[1],
          style: mono.copyWith(
            fontSize: (style.fontSize ?? 14) * .9,
            backgroundColor: colors.surfaceContainerHighest,
          ),
        ),
      );
    } else if (m[2] != null || m[3] != null) {
      spans.add(
        inline(
          context,
          m[2] ?? m[3]!,
          style.copyWith(fontWeight: FontWeight.w700),
        ),
      );
    } else if (m[4] != null || m[5] != null) {
      spans.add(
        TextSpan(
          text: m[4] ?? m[5],
          style: const TextStyle(fontStyle: FontStyle.italic),
        ),
      );
    } else {
      // No browser is opened from here: the address is shown to copy.
      spans.add(
        TextSpan(
          text: m[6] == m[7] ? m[6] : '${m[6]} (${m[7]})',
          style: TextStyle(
            color: colors.primary,
            decoration: TextDecoration.underline,
          ),
        ),
      );
    }
    at = m.end;
  }
  if (at < text.length) spans.add(TextSpan(text: text.substring(at)));
  return TextSpan(style: style, children: spans);
}
