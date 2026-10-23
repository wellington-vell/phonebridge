// Segmented LED programme meter: peak level in dBFS, with meter ballistics
// (instant rise, smooth fall) and a peak-hold segment.
window.Meter = (function () {
  const SEGMENTS = 30;
  const FLOOR = -60; // dBFS at the bottom of the scale
  const FALL = 24; // dB per second
  const HOLD = 1200; // ms the peak segment stays put
  const PEAK_FALL = 12; // dB per second once the hold is over

  let cells = [];
  let readout = null;
  let target = FLOOR;
  let shown = FLOOR;
  let peak = FLOOR;
  let peakAt = 0;
  let last = 0;
  let frame = 0;

  const toDb = (level) => (level > 0 ? Math.max(FLOOR, 20 * Math.log10(level)) : FLOOR);
  const index = (db) => Math.round(((db - FLOOR) / -FLOOR) * SEGMENTS);

  function init(bar, out) {
    readout = out;
    cells = [];
    for (let i = 0; i < SEGMENTS; i++) {
      const top = FLOOR + ((i + 1) * -FLOOR) / SEGMENTS;
      const cell = document.createElement("i");
      cell.dataset.zone = top > -6 ? "hot" : top > -18 ? "warm" : "ok";
      cells.push(cell);
    }
    bar.replaceChildren(...cells);
    paint();
  }

  function paint() {
    const lit = index(shown);
    const p = peak > FLOOR ? index(peak) - 1 : -1;
    cells.forEach((cell, i) => {
      cell.classList.toggle("on", i < lit);
      cell.classList.toggle("peak", i === p && i >= lit);
    });
    if (readout) readout.textContent = peak > FLOOR ? `Peak ${peak.toFixed(1)} dB` : "Peak −∞";
  }

  function tick(now) {
    const dt = last ? (now - last) / 1000 : 0;
    last = now;
    shown = Math.max(target, shown - FALL * dt);
    if (now - peakAt > HOLD) peak = Math.max(shown, peak - PEAK_FALL * dt);
    paint();
    if (shown > target || peak > shown) frame = requestAnimationFrame(tick);
    else {
      frame = 0;
      last = 0;
    }
  }

  /** Feed a new peak level (0..1, linear). */
  function set(level) {
    target = toDb(level);
    const now = performance.now();
    if (target >= shown) shown = target;
    if (target >= peak) {
      peak = target;
      peakAt = now;
    }
    paint();
    if (!frame) frame = requestAnimationFrame(tick);
  }

  return { init, set };
})();
