import { describe, expect, it, vi } from "vitest";
import { bindActions, h } from "./dom";
import { icon } from "./icons";

describe("h()", () => {
  it("writes strings as text, never as markup (REQ-S-016, REQ-S-018)", () => {
    const hostile = '<img src=x onerror="alert(1)"><script>alert(2)</script>';
    const el = h("div", { title: hostile }, hostile, 42);
    expect(el.textContent).toBe(`${hostile}42`);
    expect(el.querySelector("img, script")).toBeNull();
    expect(el.getAttribute("title")).toBe(hostile);
  });

  it("refuses inline event handler attributes", () => {
    expect(() => h("button", { onclick: "alert(1)" })).toThrow(/refused/);
    expect(() => h("img", { OnError: "alert(1)" })).toThrow(/refused/);
  });

  it("drops false, null and undefined attributes and children", () => {
    const el = h(
      "button",
      { disabled: false, hidden: true, "data-arg": undefined },
      null,
      false,
      undefined,
      ["a", ["b"]],
    );
    expect(el.hasAttribute("disabled")).toBe(false);
    expect(el.getAttribute("hidden")).toBe("");
    expect(el.hasAttribute("data-arg")).toBe(false);
    expect(el.textContent).toBe("ab");
  });
});

describe("icon()", () => {
  it("builds a decorative inline SVG", () => {
    const svg = icon("check", "sm");
    expect(svg.namespaceURI).toBe("http://www.w3.org/2000/svg");
    expect(svg.getAttribute("aria-hidden")).toBe("true");
    expect(svg.getAttribute("class")).toBe("ic sm");
    expect(svg.querySelector("path")).not.toBeNull();
  });
});

describe("bindActions()", () => {
  it("dispatches data-action with data-arg and ignores disabled controls", () => {
    const root = h(
      "div",
      null,
      h("button", { "data-action": "open", "data-arg": "s1" }, h("span", null, "Open")),
      h("button", { "data-action": "stop", disabled: true }, "Stop"),
    );
    document.body.append(root);
    const handler = vi.fn();
    const off = bindActions(root, handler);
    (root.querySelector("span") as HTMLElement).click();
    (root.querySelectorAll("button")[1] as HTMLElement).click();
    expect(handler).toHaveBeenCalledTimes(1);
    expect(handler.mock.calls[0]?.[0]).toMatchObject({ action: "open", arg: "s1" });
    off();
    root.remove();
  });
});
