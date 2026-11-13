// The icon set of the approved UI mock (ST-M009-008), shared by both roles.
//
// Each icon is a 24x24 stroke drawing. `icon()` builds it as an inline SVG
// element with DOM APIs (`createElementNS`), never from markup, and marks it
// `aria-hidden`: every icon sits next to a text label, so it never carries
// meaning on its own (REQ-N-013).

const SVG_NS = "http://www.w3.org/2000/svg";

type IconPart = readonly ["path" | "circle" | "rect", Readonly<Record<string, string>>];

export const ICONS = {
  link: [
    ["path", { d: "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1" }],
    ["path", { d: "M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1" }],
  ],
  home: [["path", { d: "M3 10.5 12 3l9 7.5V20a1 1 0 0 1-1 1h-5v-6H9v6H4a1 1 0 0 1-1-1z" }]],
  history: [
    ["path", { d: "M3 12a9 9 0 1 0 3-6.7L3 8" }],
    ["path", { d: "M3 3v5h5" }],
    ["path", { d: "M12 7v5l3 2" }],
  ],
  spark: [
    ["path", { d: "M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9z" }],
    ["path", { d: "M19 16l.7 1.8 1.8.7-1.8.7L19 21l-.7-1.8-1.8-.7 1.8-.7z" }],
  ],
  gear: [
    ["circle", { cx: "12", cy: "12", r: "3" }],
    [
      "path",
      {
        d: "M12 2v3M12 19v3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M2 12h3M19 12h3M4.9 19.1 7 17M17 7l2.1-2.1",
      },
    ],
  ],
  life: [
    ["circle", { cx: "12", cy: "12", r: "9" }],
    ["circle", { cx: "12", cy: "12", r: "4" }],
    ["path", { d: "M5.6 5.6l3.6 3.6M14.8 14.8l3.6 3.6M18.4 5.6l-3.6 3.6M9.2 14.8l-3.6 3.6" }],
  ],
  tool: [
    [
      "path",
      {
        d: "M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18l3 3 6.3-6.3a4 4 0 0 0 5.4-5.4l-2.6 2.6-2.4-.6-.6-2.4z",
      },
    ],
  ],
  term: [
    ["rect", { x: "3", y: "4", width: "18", height: "16", rx: "2" }],
    ["path", { d: "M7 9l3 3-3 3M13 15h4" }],
  ],
  out: [
    ["path", { d: "M12 15V3M7 8l5-5 5 5" }],
    ["path", { d: "M5 14v5a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-5" }],
  ],
  in: [
    ["path", { d: "M12 3v12M7 10l5 5 5-5" }],
    ["path", { d: "M5 14v5a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-5" }],
  ],
  eye: [
    ["path", { d: "M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z" }],
    ["circle", { cx: "12", cy: "12", r: "3" }],
  ],
  cursor: [["path", { d: "M5 3l14 8-6 2-2 6z" }]],
  check: [["path", { d: "M5 12.5l4.5 4.5L19 7" }]],
  alert: [
    ["path", { d: "M12 3l10 18H2z" }],
    ["path", { d: "M12 10v4M12 17.5v.01" }],
  ],
  x: [["path", { d: "M6 6l12 12M18 6 6 18" }]],
  shield: [
    ["path", { d: "M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z" }],
    ["path", { d: "M9 12l2 2 4-4" }],
  ],
  folder: [
    ["path", { d: "M3 6a1 1 0 0 1 1-1h5l2 2h9a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z" }],
  ],
  wifi: [
    [
      "path",
      { d: "M2 9a14 14 0 0 1 20 0M5 12.5a10 10 0 0 1 14 0M8.5 16a5 5 0 0 1 7 0M12 19.5v.01" },
    ],
  ],
  stop: [["rect", { x: "6", y: "6", width: "12", height: "12", rx: "2", fill: "currentColor" }]],
  user: [
    ["circle", { cx: "12", cy: "8", r: "4" }],
    ["path", { d: "M4 21a8 8 0 0 1 16 0" }],
  ],
  bot: [
    ["rect", { x: "4", y: "8", width: "16", height: "12", rx: "3" }],
    ["path", { d: "M12 4v4M9 13v1M15 13v1M2 13v3M22 13v3" }],
  ],
  desktop: [
    ["rect", { x: "3", y: "4", width: "18", height: "12", rx: "2" }],
    ["path", { d: "M8 20h8M12 16v4" }],
  ],
  key: [
    ["rect", { x: "2", y: "6", width: "20", height: "12", rx: "2" }],
    ["path", { d: "M6 10h.01M10 10h.01M14 10h.01M18 10h.01M7 14h10" }],
  ],
  restart: [
    ["path", { d: "M3 12a9 9 0 1 0 3-6.7L3 8" }],
    ["path", { d: "M3 3v5h5" }],
  ],
  lock: [
    ["rect", { x: "4", y: "11", width: "16", height: "10", rx: "2" }],
    ["path", { d: "M8 11V7a4 4 0 0 1 8 0v4" }],
  ],
  doc: [
    ["path", { d: "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z" }],
    ["path", { d: "M14 3v5h5M9 13h6M9 17h6" }],
  ],
  minimize: [["path", { d: "M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7" }]],
  gauge: [
    ["path", { d: "M12 14l4-4" }],
    ["path", { d: "M3.3 17a9 9 0 1 1 17.4 0" }],
  ],
  // The file explorer (ST-M010-002).
  back: [["path", { d: "M19 12H5M11 6l-6 6 6 6" }]],
  forward: [["path", { d: "M5 12h14M13 6l6 6-6 6" }]],
  up: [["path", { d: "M12 19V5M6 11l6-6 6 6" }]],
  drive: [
    ["path", { d: "M5.5 13 8 5h8l2.5 8" }],
    ["rect", { x: "3", y: "13", width: "18", height: "7", rx: "2" }],
    ["path", { d: "M17 16.5v.01" }],
  ],
  file: [
    ["path", { d: "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z" }],
    ["path", { d: "M14 3v5h5" }],
  ],
  pencil: [["path", { d: "M4 20h4L19 9l-4-4L4 16zM13.5 6.5l4 4" }]],
} as const satisfies Record<string, readonly IconPart[]>;

export type IconName = keyof typeof ICONS;

/** An inline SVG icon (`.ic`, plus an optional size class such as `sm`). */
export function icon(name: IconName, size?: "sm" | "lg" | "xl"): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", size ? `ic ${size}` : "ic");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  for (const [tag, attrs] of ICONS[name] as readonly IconPart[]) {
    const part = document.createElementNS(SVG_NS, tag);
    for (const [key, value] of Object.entries(attrs)) part.setAttribute(key, value);
    svg.append(part);
  }
  return svg;
}
