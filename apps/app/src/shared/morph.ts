// In-place DOM patching for the role interfaces (ST-M009-008).
//
// Each screen is a pure function of a state snapshot that returns a fresh
// element tree. Replacing the live tree with it on every change would drop
// keyboard focus, reset scroll positions and re-announce live regions, and it
// would destroy the embedded terminal. `morph()` instead patches the live tree
// to match the new one:
//
// - nodes of the same kind are kept and updated (attributes, text, children);
// - children with a `data-key` are matched by key, so a feed that grows at the
//   top keeps its existing items (and their focus) in place;
// - an element with `data-keep` is left untouched once it exists with the same
//   value, so a controller can own what is inside it (the xterm terminal).
//
// It only copies attributes and text nodes, so it adds nothing that `h()`
// could not build (no markup is ever parsed).

/** Patches `live` so it matches `next`, and returns the node now in place. */
export function morph(live: Node, next: Node): Node {
  if (!sameKind(live, next)) {
    live.parentNode?.replaceChild(next, live);
    return next;
  }
  if (live.nodeType === Node.TEXT_NODE || live.nodeType === Node.COMMENT_NODE) {
    if (live.nodeValue !== next.nodeValue) live.nodeValue = next.nodeValue;
    return live;
  }
  const from = live as Element;
  const to = next as Element;
  const keep = to.getAttribute("data-keep");
  if (keep !== null && from.getAttribute("data-keep") === keep) {
    syncAttributes(from, to);
    return from;
  }
  syncAttributes(from, to);
  morphChildren(from, to);
  return from;
}

/** Patches the children of `live` to match those of `next`. */
export function morphChildren(live: Element, next: Element): void {
  const keyed = new Map<string, Element>();
  for (const child of Array.from(live.children)) {
    const key = child.getAttribute("data-key");
    if (key !== null) keyed.set(key, child);
  }
  const wanted = Array.from(next.childNodes);
  for (let index = 0; index < wanted.length; index += 1) {
    const target = wanted[index];
    if (!target) continue;
    const current: ChildNode | null = live.childNodes[index] ?? null;
    const key = target instanceof Element ? target.getAttribute("data-key") : null;
    if (key !== null) {
      const match = keyed.get(key);
      if (match && sameKind(match, target)) {
        keyed.delete(key);
        if (match !== current) live.insertBefore(match, current);
        morph(match, target);
        continue;
      }
      live.insertBefore(target, current);
      continue;
    }
    const currentKey = current instanceof Element ? current.getAttribute("data-key") : null;
    if (current && currentKey === null && sameKind(current, target)) {
      morph(current, target);
    } else {
      live.insertBefore(target, current);
    }
  }
  while (live.childNodes.length > wanted.length) {
    live.lastChild?.remove();
  }
}

function sameKind(a: Node, b: Node): boolean {
  if (a.nodeType !== b.nodeType) return false;
  if (a instanceof Element && b instanceof Element) {
    return (
      a.namespaceURI === b.namespaceURI &&
      a.tagName === b.tagName &&
      a.getAttribute("data-key") === b.getAttribute("data-key") &&
      a.getAttribute("data-keep") === b.getAttribute("data-keep")
    );
  }
  return true;
}

function syncAttributes(from: Element, to: Element): void {
  for (const attr of Array.from(from.attributes)) {
    if (!to.hasAttribute(attr.name)) from.removeAttribute(attr.name);
  }
  for (const attr of Array.from(to.attributes)) {
    if (from.getAttribute(attr.name) !== attr.value) from.setAttribute(attr.name, attr.value);
  }
}
