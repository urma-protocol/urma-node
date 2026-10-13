import { LogicalSize } from "@tauri-apps/api/dpi";
import { currentMonitor, getCurrentWindow } from "@tauri-apps/api/window";

type Mode = "simple" | "advanced";

const key = "urma-node-mode";
const baseline = 1080;
const step = 0.25;
const damping = 0.5;
const simple = { width: 480, height: 680 };
const advanced = { width: 1100, height: 760 };
const advancedMin = { width: 720, height: 520 };

const read = (): Mode => {
  try {
    return localStorage.getItem(key) === "advanced" ? "advanced" : "simple";
  } catch {
    return "simple";
  }
};

const write = (value: Mode) => {
  try {
    localStorage.setItem(key, value);
  } catch {}
};

const appWindow = getCurrentWindow();
let mode = $state<Mode>("simple");
let scale = 0;
let pending = false;

const monitorScale = async () => {
  const monitor = await currentMonitor();
  if (!monitor) return 1;
  const logical = monitor.size.height / monitor.scaleFactor;
  const ratio = 1 + (logical / baseline - 1) * damping;
  return Math.max(1, Math.floor(ratio / step) * step);
};

const sized = (size: { width: number; height: number }) =>
  new LogicalSize(size.width * scale, size.height * scale);

const apply = async () => {
  document.documentElement.style.setProperty("--scale", String(scale));
  await appWindow.setMinSize(null);
  await appWindow.setMaxSize(null);
  if (mode === "simple") {
    await appWindow.setSize(sized(simple));
    await appWindow.setMinSize(sized(simple));
    await appWindow.setMaxSize(sized(simple));
  } else {
    await appWindow.setSize(sized(advanced));
    await appWindow.setMinSize(sized(advancedMin));
  }
};

const refit = async () => {
  pending = false;
  const next = await monitorScale();
  if (next === scale) return;
  scale = next;
  await apply();
};

const schedule = () => {
  if (pending) return;
  pending = true;
  requestAnimationFrame(refit);
};

export const layout = {
  get mode(): Mode {
    return mode;
  },
  async toggle() {
    mode = mode === "simple" ? "advanced" : "simple";
    write(mode);
    await apply();
    await appWindow.center();
  },
};

export const openWindow = async () => {
  try {
    scale = await monitorScale();
    await apply();
    await appWindow.center();
  } finally {
    await appWindow.show();
  }
  if (read() === "advanced") await layout.toggle();
  await appWindow.onMoved(schedule);
  await appWindow.onScaleChanged(schedule);
};
