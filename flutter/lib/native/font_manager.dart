import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import '../common.dart';

const _kCjkFontFamily = 'SystemCJK';

// Well-known paths for CJK fonts on common Linux distributions.
// Used as a fallback when fc-list is unavailable.
const _kCjkFontSearchPaths = [
  // Debian / Ubuntu — noto-fonts package
  '/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc',
  '/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc',
  // Ubuntu split packages (noto-fonts-cjk)
  '/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf',
  '/usr/share/fonts/opentype/noto/NotoSansCJKtc-Regular.otf',
  '/usr/share/fonts/opentype/noto/NotoSansCJKjp-Regular.otf',
  '/usr/share/fonts/opentype/noto/NotoSansCJKkr-Regular.otf',
  // Fedora / RHEL / Rocky
  '/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc',
  '/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc',
  // Arch Linux
  '/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc',
  '/usr/share/fonts/noto-cjk/NotoSansCJKsc-Regular.otf',
  // Generic / others
  '/usr/share/fonts/noto/NotoSansCJK-Regular.ttc',
  '/usr/share/fonts/noto/NotoSansCJKsc-Regular.otf',
  // WenQuanYi — often pre-installed on CJK distros
  '/usr/share/fonts/truetype/wqy/wqy-microhei.ttc',
  '/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc',
  '/usr/share/fonts/wqy-microhei/wqy-microhei.ttc',
  '/usr/share/fonts/wqy-zenhei/wqy-zenhei.ttc',
];

Future<bool> _isLinuxArm64() async {
  if (!Platform.isLinux) return false;
  try {
    final result = await Process.run('uname', ['-m']);
    final arch = result.stdout.toString().trim();
    return arch == 'aarch64' || arch == 'arm64';
  } catch (_) {
    return false;
  }
}

// Ask fontconfig (as a CLI tool) for a CJK font file. This works even
// when the Flutter engine was built without --enable-fontconfig, because
// fontconfig is a system library/tool independent of the engine.
Future<String?> _findCjkFontViaFcList() async {
  for (final lang in ['zh', 'zh-cn', 'zh-tw', 'ja', 'ko']) {
    try {
      final result = await Process.run(
        'fc-list',
        [':lang=$lang', '--format=%{file}\n'],
      );
      if (result.exitCode != 0) continue;
      for (final line in result.stdout.toString().split('\n')) {
        final path = line.trim();
        if (path.isNotEmpty && File(path).existsSync()) {
          return path;
        }
      }
    } catch (_) {}
  }
  return null;
}

Future<String?> _findCjkFontPath() async {
  final fcPath = await _findCjkFontViaFcList();
  if (fcPath != null) return fcPath;

  for (final path in _kCjkFontSearchPaths) {
    if (File(path).existsSync()) return path;
  }
  return null;
}

void _applyThemeFontFallback() {
  final fallbacks = [_kCjkFontFamily];
  MyTheme.lightTheme = MyTheme.lightTheme.copyWith(
    textTheme: MyTheme.lightTheme.textTheme.apply(
      fontFamilyFallback: fallbacks,
    ),
    primaryTextTheme: MyTheme.lightTheme.primaryTextTheme.apply(
      fontFamilyFallback: fallbacks,
    ),
  );
  MyTheme.darkTheme = MyTheme.darkTheme.copyWith(
    textTheme: MyTheme.darkTheme.textTheme.apply(
      fontFamilyFallback: fallbacks,
    ),
    primaryTextTheme: MyTheme.darkTheme.primaryTextTheme.apply(
      fontFamilyFallback: fallbacks,
    ),
  );
}

/// On ARM64 Linux with flutter-elinux the engine is built without
/// --enable-fontconfig, so the engine cannot discover system fonts.
/// This function bypasses that limitation by loading a CJK font from
/// a known filesystem path and registering it as a theme-level fallback
/// so that CJK characters render correctly across the whole app.
Future<void> loadSystemCJKFonts() async {
  if (!await _isLinuxArm64()) return;

  final fontPath = await _findCjkFontPath();
  if (fontPath == null) {
    debugPrint('ARM64 Linux: no CJK font found; CJK text may not render');
    return;
  }

  try {
    final loader = FontLoader(_kCjkFontFamily);
    final bytes = await File(fontPath).readAsBytes();
    loader.addFont(Future.value(ByteData.view(bytes.buffer)));
    await loader.load();
    debugPrint('ARM64 Linux: loaded CJK font from $fontPath');
    _applyThemeFontFallback();
  } catch (e) {
    debugPrint('ARM64 Linux: failed to load CJK font: $e');
  }
}
