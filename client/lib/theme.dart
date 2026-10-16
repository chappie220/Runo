import 'package:flutter/material.dart';

/// The app's look: Material 3 in a technology blue on metal grey, light
/// or dark as the system is.
const seed = Color(0xFF0A6CFF);

/// Device IDs, codes, logs: one family on every platform.
const mono = TextStyle(
  fontFamily: 'monospace',
  fontFamilyFallback: ['Cascadia Mono', 'Consolas', 'DejaVu Sans Mono'],
);

/// The blue kept close to [seed], the surfaces and the second colour
/// a cool, untinted grey.
ColorScheme scheme(Brightness brightness) {
  final blue = ColorScheme.fromSeed(
    seedColor: seed,
    brightness: brightness,
    dynamicSchemeVariant: DynamicSchemeVariant.fidelity,
  );
  if (brightness == Brightness.dark) {
    return blue.copyWith(
      secondary: const Color(0xFFB3BDC7),
      onSecondary: const Color(0xFF1D252D),
      secondaryContainer: const Color(0xFF343C45),
      onSecondaryContainer: const Color(0xFFD9E1E9),
      surface: const Color(0xFF111316),
      onSurface: const Color(0xFFE2E5E9),
      onSurfaceVariant: const Color(0xFFA9B1BA),
      surfaceContainerLowest: const Color(0xFF0C0E10),
      surfaceContainerLow: const Color(0xFF181B1F),
      surfaceContainer: const Color(0xFF1D2125),
      surfaceContainerHigh: const Color(0xFF24282D),
      surfaceContainerHighest: const Color(0xFF2D3238),
      outline: const Color(0xFF7F8891),
      outlineVariant: const Color(0xFF3A4047),
      surfaceTint: Colors.transparent,
    );
  }
  return blue.copyWith(
    secondary: const Color(0xFF5B6772),
    onSecondary: Colors.white,
    secondaryContainer: const Color(0xFFDDE3E9),
    onSecondaryContainer: const Color(0xFF29323B),
    surface: const Color(0xFFF5F7F9),
    onSurface: const Color(0xFF191C20),
    onSurfaceVariant: const Color(0xFF545C64),
    surfaceContainerLowest: Colors.white,
    surfaceContainerLow: const Color(0xFFEEF1F4),
    surfaceContainer: const Color(0xFFE8EBEF),
    surfaceContainerHigh: const Color(0xFFE1E5E9),
    surfaceContainerHighest: const Color(0xFFDADFE4),
    outline: const Color(0xFF8A929A),
    outlineVariant: const Color(0xFFC9CED4),
    surfaceTint: Colors.transparent,
  );
}

ThemeData theme(Brightness brightness) {
  final colors = scheme(brightness);
  final base = ThemeData(colorScheme: colors, useMaterial3: true);
  final rounded = RoundedRectangleBorder(
    borderRadius: BorderRadius.circular(16),
  );
  const buttonSize = Size(64, 44);
  return base.copyWith(
    scaffoldBackgroundColor: colors.surface,
    appBarTheme: AppBarTheme(
      backgroundColor: colors.surface,
      scrolledUnderElevation: 2,
      surfaceTintColor: colors.surfaceTint,
      titleTextStyle: TextStyle(
        fontSize: 20,
        fontWeight: FontWeight.w600,
        color: colors.onSurface,
      ),
    ),
    cardTheme: CardThemeData(
      elevation: 0,
      margin: EdgeInsets.zero,
      color: colors.surfaceContainerLow,
      shape: rounded.copyWith(
        side: BorderSide(color: colors.outlineVariant.withValues(alpha: .6)),
      ),
      clipBehavior: Clip.antiAlias,
    ),
    listTileTheme: ListTileThemeData(shape: rounded),
    filledButtonTheme: FilledButtonThemeData(
      style: FilledButton.styleFrom(minimumSize: buttonSize),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(minimumSize: buttonSize),
    ),
    inputDecorationTheme: InputDecorationTheme(
      filled: true,
      fillColor: colors.surfaceContainerHighest.withValues(alpha: .5),
      border: UnderlineInputBorder(
        borderRadius: BorderRadius.circular(12),
        borderSide: BorderSide.none,
      ),
      enabledBorder: UnderlineInputBorder(
        borderRadius: BorderRadius.circular(12),
        borderSide: BorderSide.none,
      ),
      focusedBorder: UnderlineInputBorder(
        borderRadius: BorderRadius.circular(12),
        borderSide: BorderSide(color: colors.primary, width: 2),
      ),
    ),
    dialogTheme: DialogThemeData(
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(24)),
    ),
    snackBarTheme: SnackBarThemeData(
      behavior: SnackBarBehavior.floating,
      shape: rounded.copyWith(borderRadius: BorderRadius.circular(12)),
    ),
    floatingActionButtonTheme: FloatingActionButtonThemeData(
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(16)),
    ),
    navigationRailTheme: NavigationRailThemeData(
      backgroundColor: colors.surface,
      labelType: NavigationRailLabelType.all,
    ),
  );
}

/// The green of "connected" and "running", which Material leaves out.
Color good(BuildContext context) =>
    Theme.of(context).brightness == Brightness.dark
    ? const Color(0xFF7DD99A)
    : const Color(0xFF1B8A47);
