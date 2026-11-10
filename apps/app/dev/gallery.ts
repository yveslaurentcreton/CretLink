// Development-only gallery of both roles' screens (ST-M009-008,
// ST-M009-009): every snapshot fixture rendered with the real views and
// styles, without Tauri, for visual comparison with the approved mock.
// Technician fixtures are `?f=<name>`, customer fixtures `?c=<name>` (the
// compact bar's are `?c=mini-…`).

import "../src/base.css";
import "../src/technician/styles.css";
import "../src/customer/styles.css";
import { FIXTURES as CUSTOMER, MINI_FIXTURES } from "../src/customer/fixtures";
import { renderMini } from "../src/customer/views/mini";
import { renderCustomer } from "../src/customer/views/shell";
import { h } from "../src/shared/dom";
import { FIXTURES } from "../src/technician/fixtures";
import { renderTechnician } from "../src/technician/views/shell";

const root = document.getElementById("app");
const params = new URLSearchParams(window.location.search);
const name = params.get("f");
const fixture = name ? FIXTURES[name] : undefined;
const customerName = params.get("c");
const customer = customerName ? (CUSTOMER[customerName] ?? MINI_FIXTURES[customerName]) : undefined;

if (root && customer) {
  const mini = customer.window === "mini";
  document.documentElement.classList.toggle("mini-window", mini);
  root.replaceChildren(mini ? renderMini(customer) : renderCustomer(customer));
} else if (root && fixture) {
  const screen = renderTechnician(fixture);
  // A static stand-in for the live xterm, which needs the backend.
  const body = screen.querySelector('[data-keep="terminal"]');
  body?.append(
    h(
      "pre",
      { class: "gallery-term" },
      "PS C:\\Windows\\system32> Get-PnpDevice -Status ERROR | Select FriendlyName\n\n",
      "FriendlyName\n------------\nIntel(R) Wi-Fi 6E AX211 160MHz\n\nPS C:\\Windows\\system32> ",
    ),
  );
  screen
    .querySelector('[data-keep="terminal-badge"]')
    ?.append(h("span", { class: "sh-badge" }, "Terminal live"));
  root.replaceChildren(screen);
} else if (root) {
  root.replaceChildren(
    h(
      "div",
      { class: "cl-ui tech", style: "padding:24px" },
      h("h2", null, "Technician screens"),
      h(
        "ul",
        null,
        Object.keys(FIXTURES).map((key) =>
          h("li", null, h("a", { href: `?f=${encodeURIComponent(key)}` }, key)),
        ),
      ),
      h("h2", null, "Customer screens"),
      h(
        "ul",
        null,
        [...Object.keys(CUSTOMER), ...Object.keys(MINI_FIXTURES)].map((key) =>
          h("li", null, h("a", { href: `?c=${encodeURIComponent(key)}` }, key)),
        ),
      ),
    ),
  );
}
