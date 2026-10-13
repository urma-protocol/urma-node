type Theme = "light" | "dark";

const key = "urma-node-theme";
const query = matchMedia("(prefers-color-scheme: dark)");

const read = (): Theme | null => {
  try {
    const value = localStorage.getItem(key);
    return value === "light" || value === "dark" ? value : null;
  } catch {
    return null;
  }
};

const write = (value: Theme | null) => {
  try {
    if (value) localStorage.setItem(key, value);
    else localStorage.removeItem(key);
  } catch {}
};

const apply = (value: Theme | null) => {
  if (value) document.documentElement.dataset.theme = value;
  else delete document.documentElement.dataset.theme;
};

let system = $state<Theme>(query.matches ? "dark" : "light");
const stored = read();
let override = $state<Theme | null>(stored);

query.addEventListener("change", (event) => {
  system = event.matches ? "dark" : "light";
});
apply(stored);

export const theme = {
  get current(): Theme {
    return override ?? system;
  },
  toggle() {
    const next: Theme = this.current === "dark" ? "light" : "dark";
    override = next === system ? null : next;
    write(override);
    apply(override);
  },
};
