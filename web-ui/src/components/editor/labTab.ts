// The lab page's active tab, module-scoped so affordances elsewhere (Edit
// config, Edit playbook) can land on a specific tab without threading
// callbacks through the component tree.

import { createSignal } from "solid-js";

export type LabTab = "design" | "files" | "logs";

export const [labTab, setLabTab] = createSignal<LabTab>("design");
