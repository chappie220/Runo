import 'package:flutter/material.dart';

import 'problem.dart';
import 'text.dart';
import 'theme.dart';

/// Keeps [child] to a readable width in the middle of a wide window.
class Content extends StatelessWidget {
  const Content({super.key, required this.child, this.width = 960});

  final Widget child;
  final double width;

  @override
  Widget build(BuildContext context) => Align(
    alignment: Alignment.topCenter,
    child: ConstrainedBox(
      constraints: BoxConstraints(maxWidth: width),
      child: child,
    ),
  );
}

/// A small coloured label for a state: online, pairing open, running.
class Pill extends StatelessWidget {
  const Pill(this.label, {super.key, required this.color, this.icon});

  final String label;
  final Color color;
  final IconData? icon;

  @override
  Widget build(BuildContext context) {
    final style = Theme.of(context).textTheme.labelMedium;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(
        color: color.withValues(alpha: .12),
        borderRadius: BorderRadius.circular(99),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (icon == null)
            Container(
              width: 7,
              height: 7,
              decoration: BoxDecoration(color: color, shape: BoxShape.circle),
            )
          else
            Icon(icon, size: 14, color: color),
          const SizedBox(width: 6),
          Flexible(
            child: Text(
              label,
              overflow: TextOverflow.ellipsis,
              style: style?.copyWith(color: color, fontWeight: FontWeight.w600),
            ),
          ),
        ],
      ),
    );
  }
}

/// The rounded square an item's icon sits in.
class Tile extends StatelessWidget {
  const Tile(this.icon, {super.key, this.active = true, this.size = 44});

  final IconData icon;
  final bool active;
  final double size;

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    return Container(
      width: size,
      height: size,
      decoration: BoxDecoration(
        color: active
            ? colors.primaryContainer
            : colors.surfaceContainerHighest,
        borderRadius: BorderRadius.circular(size * .3),
      ),
      child: Icon(
        icon,
        size: size * .5,
        color: active ? colors.onPrimaryContainer : colors.onSurfaceVariant,
      ),
    );
  }
}

/// A section's title, with what goes beside it (a count, a spinner).
class Heading extends StatelessWidget {
  const Heading(this.text, {super.key, this.trailing});

  final String text;
  final Widget? trailing;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(4, 24, 4, 12),
      child: Row(
        children: [
          Text(
            text,
            style: theme.textTheme.titleMedium?.copyWith(
              fontWeight: FontWeight.w600,
            ),
          ),
          if (trailing != null) ...[const SizedBox(width: 10), trailing!],
        ],
      ),
    );
  }
}

/// Where a list would be when it is empty.
class Empty extends StatelessWidget {
  const Empty({super.key, required this.icon, required this.text, this.action});

  final IconData icon;
  final String text;
  final Widget? action;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Card(
      color: theme.colorScheme.surfaceContainerLowest,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 28),
        child: Column(
          children: [
            Icon(icon, size: 36, color: theme.colorScheme.outline),
            const SizedBox(height: 12),
            Text(
              text,
              textAlign: TextAlign.center,
              style: theme.textTheme.bodyMedium?.copyWith(
                color: theme.colorScheme.onSurfaceVariant,
              ),
            ),
            if (action != null) ...[const SizedBox(height: 12), action!],
          ],
        ),
      ),
    );
  }
}

/// A [problem] in place of a page: what went wrong in a sentence, a
/// [hint] on what to do, the device's own words folded away, and a way
/// to try again.
class ProblemView extends StatelessWidget {
  const ProblemView({
    super.key,
    required this.problem,
    this.hint,
    this.icon = Icons.cloud_off_outlined,
    this.retry,
    this.retryLabel,
  });

  final Problem problem;
  final String? hint;
  final IconData icon;
  final VoidCallback? retry;
  final String? retryLabel;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    final t = T.of;
    return Center(
      child: SingleChildScrollView(
        padding: const EdgeInsets.all(24),
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 460),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Container(
                width: 64,
                height: 64,
                decoration: BoxDecoration(
                  color: colors.errorContainer,
                  shape: BoxShape.circle,
                ),
                child: Icon(icon, size: 30, color: colors.onErrorContainer),
              ),
              const SizedBox(height: 20),
              Text(
                problem.title,
                textAlign: TextAlign.center,
                style: theme.textTheme.titleMedium?.copyWith(
                  fontWeight: FontWeight.w600,
                ),
              ),
              if (hint != null) ...[
                const SizedBox(height: 10),
                Text(
                  hint!,
                  textAlign: TextAlign.center,
                  style: theme.textTheme.bodyMedium?.copyWith(
                    color: colors.onSurfaceVariant,
                  ),
                ),
              ],
              if (retry != null) ...[
                const SizedBox(height: 24),
                FilledButton.icon(
                  icon: const Icon(Icons.refresh),
                  label: Text(retryLabel ?? t.tryAgain),
                  onPressed: retry,
                ),
              ],
              if (problem.details != null) ...[
                const SizedBox(height: 16),
                Details(problem.details!),
              ],
            ],
          ),
        ),
      ),
    );
  }
}

/// The device's own words for an error, folded away until asked for.
class Details extends StatelessWidget {
  const Details(this.text, {super.key});

  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Theme(
      data: theme.copyWith(dividerColor: Colors.transparent),
      child: ExpansionTile(
        dense: true,
        tilePadding: const EdgeInsets.symmetric(horizontal: 12),
        childrenPadding: const EdgeInsets.fromLTRB(12, 0, 12, 12),
        expandedAlignment: Alignment.centerLeft,
        title: Text(
          T.of.details,
          style: theme.textTheme.labelLarge?.copyWith(
            color: theme.colorScheme.onSurfaceVariant,
          ),
        ),
        children: [
          SelectableText(
            text,
            style: mono.copyWith(
              fontSize: 12,
              color: theme.colorScheme.onSurfaceVariant,
            ),
          ),
        ],
      ),
    );
  }
}

/// Text the device composes, set in a panel.
class Panel extends StatelessWidget {
  const Panel({super.key, required this.child});

  final Widget child;

  @override
  Widget build(BuildContext context) => Container(
    width: double.infinity,
    padding: const EdgeInsets.all(16),
    decoration: BoxDecoration(
      color: Theme.of(context).colorScheme.surfaceContainer,
      borderRadius: BorderRadius.circular(16),
    ),
    child: child,
  );
}

/// The columns a list of cards gets at this width.
int columns(double width) => width >= 1100
    ? 3
    : width >= 680
    ? 2
    : 1;

/// [children] in as many equal columns as [columns] gives the width.
class Cards extends StatelessWidget {
  const Cards({super.key, required this.children, this.gap = 12});

  final List<Widget> children;
  final double gap;

  @override
  Widget build(BuildContext context) => LayoutBuilder(
    builder: (context, box) {
      final n = columns(box.maxWidth);
      final width = (box.maxWidth - gap * (n - 1)) / n;
      return Wrap(
        spacing: gap,
        runSpacing: gap,
        children: [
          for (final child in children) SizedBox(width: width, child: child),
        ],
      );
    },
  );
}

/// Green when good, the theme's outline colour when idle.
Color state(BuildContext context, bool on) =>
    on ? good(context) : Theme.of(context).colorScheme.outline;

/// A pulled-to-refresh list kept to a readable width in the middle.
class Listing extends StatelessWidget {
  const Listing({super.key, required this.children, this.onRefresh});

  final List<Widget> children;
  final Future<void> Function()? onRefresh;

  @override
  Widget build(BuildContext context) {
    final list = LayoutBuilder(
      builder: (context, box) {
        final side = box.maxWidth > 992 ? (box.maxWidth - 960) / 2 : 16.0;
        return ListView(
          physics: const AlwaysScrollableScrollPhysics(),
          padding: EdgeInsets.fromLTRB(side, 16, side, 24),
          children: children,
        );
      },
    );
    final refresh = onRefresh;
    return refresh == null
        ? list
        : RefreshIndicator(onRefresh: refresh, child: list);
  }
}
