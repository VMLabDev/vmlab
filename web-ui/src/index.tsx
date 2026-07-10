/* @refresh reload */
import { render } from "solid-js/web";
// Forge CSS must load in this order: fonts → tokens → base → component layers.
import "@forge/tokens/fonts.css";
import "@forge/tokens/tokens.css";
import "@forge/tokens/base.css";
import "@forge/ui/styles.css";
import "@forge/code/styles.css";
import "@forge/desktop/styles.css";
import "./app.css";
import App from "./App";

render(() => <App />, document.getElementById("root")!);
