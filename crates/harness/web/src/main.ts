import "./theme.css";
import { mount } from "svelte";
import App from "./App.svelte";

// Dev only: ?theme=light|dark overrides the system's look.
const theme = new URLSearchParams(location.search).get("theme");
if (theme === "light" || theme === "dark") document.documentElement.dataset["theme"] = theme;

const target = document.getElementById("app");
if (!target) throw new Error("index.html: no #app element");
mount(App, { target });
