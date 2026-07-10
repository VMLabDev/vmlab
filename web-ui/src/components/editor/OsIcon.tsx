// Guest-OS icon for canvas VM nodes: Font Awesome brand marks (Windows,
// Tux, the big distros) rendered from their path data as a nested <svg>,
// with lucide fallbacks for DOS-era and unknown guests.

import { Show } from "solid-js";
import type { IconDefinition } from "@fortawesome/free-brands-svg-icons";
import {
  faCentos,
  faDebian,
  faFedora,
  faFreebsd,
  faLinux,
  faRedhat,
  faSuse,
  faUbuntu,
  faWindows,
} from "@fortawesome/free-brands-svg-icons";
import { Monitor, Terminal } from "lucide-solid";

const BRANDS: [RegExp, IconDefinition][] = [
  [/win/, faWindows],
  [/ubuntu/, faUbuntu],
  [/debian/, faDebian],
  [/fedora/, faFedora],
  [/centos/, faCentos],
  [/rhel|redhat|rocky|alma/, faRedhat],
  [/suse/, faSuse],
  [/freebsd|openbsd|netbsd/, faFreebsd],
  [/linux|alpine|arch|nix|gentoo|void|kali/, faLinux],
];

type Brand = IconDefinition | "dos" | null;

/** Classify a guest by its template/profile strings. DOS is checked before
 *  the brand table because DOS-era templates ride Windows profiles
 *  (e.g. dos-6.22 uses the `windows-9x` profile). */
function osBrand(os: string): Brand {
  const s = os.toLowerCase();
  if (s.includes("dos")) return "dos";
  for (const [re, icon] of BRANDS) {
    if (re.test(s)) return icon;
  }
  return null;
}

export default function OsIcon(props: { os: string; x: number; y: number; size?: number }) {
  const size = () => props.size ?? 18;
  const brand = () => osBrand(props.os);
  const fa = () => {
    const b = brand();
    return b && b !== "dos" ? b : null;
  };
  return (
    <g transform={`translate(${props.x} ${props.y})`} class="topo-os">
      <Show when={fa()}>
        {(def) => {
          // Read through the accessor inside the JSX so a template change
          // swaps the mark (the non-keyed Show callback only runs once).
          const d = () => {
            const p = def().icon[4];
            return Array.isArray(p) ? p.join(" ") : p;
          };
          return (
            <svg
              class="topo-os-brand"
              width={size()}
              height={size()}
              viewBox={`0 0 ${def().icon[0]} ${def().icon[1]}`}
            >
              <path d={d()} />
            </svg>
          );
        }}
      </Show>
      <Show when={brand() === "dos"}>
        <Terminal size={size()} />
      </Show>
      <Show when={brand() === null}>
        <Monitor size={size()} />
      </Show>
    </g>
  );
}
