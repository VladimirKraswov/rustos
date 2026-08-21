// Метрики Cocoa backing surface для выбранного экрана.
//
// `frame` задан в macOS points, а QEMU рисует в backing surface. Поэтому для
// integer scaling важен именно `frame * backingScaleFactor`, а не только
// маркетинговое native-разрешение панели из System Profiler.
ObjC.import("AppKit");

function run(argv) {
    const index = Number(argv[0] || 0);
    const screens = $.NSScreen.screens.js;
    if (!Number.isInteger(index) || index < 0 || index >= screens.length) {
        throw new Error(`экран ${index} отсутствует; доступно: ${screens.length}`);
    }

    const screen = screens[index];
    const frame = screen.frame;
    const backingScale = Number(screen.backingScaleFactor);
    const width = Math.round(frame.size.width * backingScale);
    const height = Math.round(frame.size.height * backingScale);
    return `${width} ${height} ${backingScale.toFixed(3)} ${screens.length}`;
}
