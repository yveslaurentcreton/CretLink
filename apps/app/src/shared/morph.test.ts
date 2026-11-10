import { describe, expect, it } from "vitest";
import { h } from "./dom";
import { morph } from "./morph";

describe("morph()", () => {
  it("keeps the focused control and updates its text", () => {
    const live = h("div", null, h("button", { id: "b" }, "One"));
    document.body.append(live);
    (live.querySelector("#b") as HTMLButtonElement).focus();
    morph(live, h("div", null, h("button", { id: "b", class: "on" }, "Two")));
    const button = live.querySelector("#b") as HTMLButtonElement;
    expect(document.activeElement).toBe(button);
    expect(button.textContent).toBe("Two");
    expect(button.className).toBe("on");
    live.remove();
  });

  it("matches keyed children, so items added at the top keep the others in place", () => {
    const live = h(
      "ol",
      null,
      h("li", { "data-key": "a" }, "A"),
      h("li", { "data-key": "b" }, "B"),
    );
    const a = live.children[0];
    const b = live.children[1];
    morph(
      live,
      h(
        "ol",
        null,
        h("li", { "data-key": "c" }, "C"),
        h("li", { "data-key": "a" }, "A2"),
        h("li", { "data-key": "b" }, "B"),
      ),
    );
    expect(Array.from(live.children).map((c) => c.textContent)).toEqual(["C", "A2", "B"]);
    expect(live.children[1]).toBe(a);
    expect(live.children[2]).toBe(b);
    morph(live, h("ol", null, h("li", { "data-key": "b" }, "B")));
    expect(live.children.length).toBe(1);
    expect(live.children[0]).toBe(b);
  });

  it("leaves a data-keep element and its contents alone", () => {
    const live = h("div", null, h("div", { "data-keep": "terminal" }));
    const slot = live.firstElementChild as HTMLElement;
    const owned = h("canvas");
    slot.append(owned);
    morph(live, h("div", null, h("div", { "data-keep": "terminal" })));
    expect(live.firstElementChild).toBe(slot);
    expect(slot.firstElementChild).toBe(owned);
  });

  it("replaces nodes of another kind", () => {
    const live = h("div", null, h("p", null, "x"));
    morph(live, h("div", null, h("section", null, "y")));
    expect(live.innerHTML).toBe("<section>y</section>");
  });

  it("keeps the scroll position of a container", () => {
    const live = h("div", null, h("ol", { class: "feed" }, h("li", { "data-key": "1" }, "1")));
    const feed = live.firstElementChild as HTMLElement;
    feed.scrollTop = 40;
    morph(
      live,
      h(
        "div",
        null,
        h(
          "ol",
          { class: "feed" },
          h("li", { "data-key": "2" }, "2"),
          h("li", { "data-key": "1" }, "1"),
        ),
      ),
    );
    expect(live.firstElementChild).toBe(feed);
    expect(feed.scrollTop).toBe(40);
  });
});
