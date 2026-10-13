import { mount } from "svelte";
import App from "./App.svelte";
import { openWindow } from "./layout.svelte";
import "./app.css";

openWindow().catch((error) => console.error("window layout", error));

export default mount(App, { target: document.getElementById("app")! });
