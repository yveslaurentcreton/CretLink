// A tiny DOM builder shared by both roles' interfaces (ST-M009-008).
//
// Text only ever becomes a `Text` node (`textContent` semantics): nothing here
// parses markup, so an operation, path, hostname, note, purpose or code from
// the remote computer or an AI can never turn into HTML (REQ-S-016,
// REQ-S-018). Event handlers are not attributes either: interactive elements
// carry `data-action` / `data-arg` and one delegated listener dispatches them
// (`bindActions`), so a re-render never leaves a stale closure behind.

export type Child = Node | string | number | null | undefined | false | readonly Child[];

export type AttrValue = string | number | boolean | null | undefined;

export type Attrs = Readonly<Record<string, AttrValue>>;

/**
 * Builds an element. `true` sets a boolean attribute, `false`/`null`/
 * `undefined` leaves it out. Inline event-handler attributes (`on…`) are
 * refused, so no string can smuggle script in through an attribute.
 */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs?: Attrs | null,
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (attrs) {
    for (const [name, value] of Object.entries(attrs)) {
      if (value === false || value === null || value === undefined) continue;
      if (/^on/i.test(name)) throw new Error(`event handler attribute refused: ${name}`);
      element.setAttribute(name, value === true ? "" : String(value));
    }
  }
  append(element, children);
  return element;
}

/** Appends children; strings and numbers become text nodes. */
export function append(parent: Node, children: readonly Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    if (Array.isArray(child)) {
      append(parent, child);
    } else if (typeof child === "string" || typeof child === "number") {
      parent.appendChild(document.createTextNode(String(child)));
    } else {
      parent.appendChild(child as Node);
    }
  }
}

/** One dispatched intent: the `data-action` of the control and its `data-arg`. */
export interface ActionEvent {
  action: string;
  arg?: string;
  element: HTMLElement;
}

/**
 * Dispatches clicks on `[data-action]` inside `root`. Disabled controls do
 * nothing. Returns a function that removes the listener.
 */
export function bindActions(root: HTMLElement, handler: (event: ActionEvent) => void): () => void {
  const listener = (event: MouseEvent): void => {
    const target = event.target;
    if (!(target instanceof Element)) return;
    const control = target.closest<HTMLElement>("[data-action]");
    if (!control || !root.contains(control)) return;
    if (control.hasAttribute("disabled") || control.getAttribute("aria-disabled") === "true")
      return;
    if (control instanceof HTMLAnchorElement) event.preventDefault();
    const arg = control.dataset.arg;
    handler(
      arg === undefined
        ? { action: control.dataset.action ?? "", element: control }
        : {
            action: control.dataset.action ?? "",
            arg,
            element: control,
          },
    );
  };
  root.addEventListener("click", listener);
  return () => root.removeEventListener("click", listener);
}
