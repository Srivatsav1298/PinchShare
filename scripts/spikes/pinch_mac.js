// Spike A (macOS, JXA): global pinch detection via NSEvent global monitor. Usage: osascript -l JavaScript pinch_mac.js
ObjC.import('AppKit'); ObjC.import('ApplicationServices'); ObjC.import('Foundation');
const log = [];
log.push('AXIsProcessTrusted=' + $.AXIsProcessTrusted());
let acc = 0, n = 0;
const mask = (1 << 30); // NSEventMaskMagnify = 1<<30
const mon = $.NSEvent.addGlobalMonitorForEventsMatchingMaskHandler(mask, (e) => {
  acc += e.magnification; n++;
  if (e.phase == 8) { console.log((acc < -0.15 ? 'PINCH-IN' : acc > 0.15 ? 'PINCH-OUT' : 'small') + ' total=' + acc.toFixed(3) + ' events=' + n); acc = 0; n = 0; }
});
log.push('monitor=' + (mon ? 'installed' : 'NULL'));
console.log(log.join(' | '));
$.NSRunLoop.currentRunLoop.runUntilDate($.NSDate.dateWithTimeIntervalSinceNow(15));
console.log('done');
