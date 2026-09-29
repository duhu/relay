// KeyboardEvent.code → macOS virtual key code (kVK_*), and the keycap text
// each one is shown with. Only keys a shortcut can reasonably use.

import type { KeyCombo, Modifier } from "./ipc";

const CODES: Record<string, [number, string]> = {
  KeyA: [0, "A"], KeyS: [1, "S"], KeyD: [2, "D"], KeyF: [3, "F"], KeyH: [4, "H"], KeyG: [5, "G"],
  KeyZ: [6, "Z"], KeyX: [7, "X"], KeyC: [8, "C"], KeyV: [9, "V"], KeyB: [11, "B"], KeyQ: [12, "Q"],
  KeyW: [13, "W"], KeyE: [14, "E"], KeyR: [15, "R"], KeyY: [16, "Y"], KeyT: [17, "T"],
  Digit1: [18, "1"], Digit2: [19, "2"], Digit3: [20, "3"], Digit4: [21, "4"], Digit6: [22, "6"],
  Digit5: [23, "5"], Equal: [24, "="], Digit9: [25, "9"], Digit7: [26, "7"], Minus: [27, "-"],
  Digit8: [28, "8"], Digit0: [29, "0"], BracketRight: [30, "]"], KeyO: [31, "O"], KeyU: [32, "U"],
  BracketLeft: [33, "["], KeyI: [34, "I"], KeyP: [35, "P"], Enter: [36, "↩"], KeyL: [37, "L"],
  KeyJ: [38, "J"], Quote: [39, "'"], KeyK: [40, "K"], Semicolon: [41, ";"], Backslash: [42, "\\"],
  Comma: [43, ","], Slash: [44, "/"], KeyN: [45, "N"], KeyM: [46, "M"], Period: [47, "."],
  Tab: [48, "⇥"], Space: [49, "Space"], Backquote: [50, "`"], Backspace: [51, "⌫"],
  F17: [64, "F17"], F18: [79, "F18"], F19: [80, "F19"], F20: [90, "F20"], F5: [96, "F5"],
  F6: [97, "F6"], F7: [98, "F7"], F3: [99, "F3"], F8: [100, "F8"], F9: [101, "F9"],
  F11: [103, "F11"], F13: [105, "F13"], F16: [106, "F16"], F14: [107, "F14"], F10: [109, "F10"],
  F12: [111, "F12"], F15: [113, "F15"], Home: [115, "↖"], PageUp: [116, "⇞"], Delete: [117, "⌦"],
  F4: [118, "F4"], End: [119, "↘"], F2: [120, "F2"], PageDown: [121, "⇟"], F1: [122, "F1"],
  ArrowLeft: [123, "←"], ArrowRight: [124, "→"], ArrowDown: [125, "↓"], ArrowUp: [126, "↑"],
};

/** F1–F20: the only keys a shortcut may use without a modifier. */
const F_KEYS = new Set([
  122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80, 90,
]);
const MOD_ORDER: Modifier[] = ["ctrl", "opt", "shift", "cmd"];
const MOD_CAPS: Record<Modifier, string> = { ctrl: "⌃", opt: "⌥", shift: "⇧", cmd: "⌘" };

/** What a keydown means while recording. */
export type Recorded = { combo: KeyCombo } | "cancel" | "ignore";

export function recordKey(e: KeyboardEvent): Recorded {
  if (e.code === "Escape") return "cancel";
  const entry = CODES[e.code];
  if (!entry) return "ignore"; // a bare modifier, or a key we cannot send
  const modifiers: Modifier[] = [];
  if (e.ctrlKey) modifiers.push("ctrl");
  if (e.altKey) modifiers.push("opt");
  if (e.shiftKey) modifiers.push("shift");
  if (e.metaKey) modifiers.push("cmd");
  const [key_code] = entry;
  if (modifiers.length === 0 && !F_KEYS.has(key_code)) return "ignore";
  return { combo: { key_code, modifiers } };
}

/** The keycaps a combo is drawn with, modifiers first in the macOS order. */
export function keycaps(combo: KeyCombo): string[] {
  const label =
    Object.values(CODES).find(([code]) => code === combo.key_code)?.[1] ?? `#${combo.key_code}`;
  return [...MOD_ORDER.filter((m) => combo.modifiers.includes(m)).map((m) => MOD_CAPS[m]), label];
}
