// The Files tab of the live session: the file explorer (ST-M010-002,
// REQ-F-028), with the files sent and received this session as its transfers
// panel (ST-M009-008).
//
// A pure function of the snapshot, like every screen. What it draws:
//
// - **Where the explorer stands.** Back, forward, up and refresh; a path bar
//   whose parts are buttons, under the customer computer's own name, and a
//   field to type a path in; the choice between Details and List.
// - **What is there.** The drives or the entries of the folder as a grid: a
//   row for each, read by a screen reader with its place among all rows.
//   Only the rows in view and a few beyond are drawn, each at its place in a
//   box as tall as the whole list, so a folder of thousands of entries costs
//   what a screenful costs.
// - **How complete that is, in words** (REQ-N-008): opening, loading, an
//   empty folder, a list cut short, a place that was refused, a session that
//   ended. Nothing looks like an empty folder that is not one.
//
// Every name, path and label comes from the customer computer and becomes a
// text node through `h()`, never markup (REQ-S-016, REQ-S-018). Rows carry no
// `data-action`: the controller reads clicks and keys on the grid itself.

import type { ActivityEntry, ExplorerDrive, ExplorerEntry } from "../../shared/contract";
import { type Child, h } from "../../shared/dom";
import { type IconName, icon } from "../../shared/icons";
import {
  type ExplorerState,
  type SortKey,
  crumbs,
  driveKindWords,
  driveName,
  indexOfKey,
  layoutOf,
  menuItems,
  modifiedWords,
  parentPlace,
  placeText,
  placeWords,
  positionOf,
  rowCount,
  sizeWords,
  statusWords,
  typeWords,
  visibleRange,
} from "../explorer";
import { type TechnicianSnapshot, formatBytes, targetName } from "../model";
import { button, resultBadge, whoChip } from "./common";

/** A toolbar button that shows an icon and says what it does in its label. */
function tool(
  action: string,
  iconName: IconName,
  label: string,
  options: { disabled?: boolean; keys?: string } = {},
): HTMLButtonElement {
  return h(
    "button",
    {
      type: "button",
      class: "btn sm ghost xp-tool",
      "data-action": action,
      "aria-label": label,
      title: label,
      "aria-keyshortcuts": options.keys,
      disabled: options.disabled,
    },
    icon(iconName, "sm"),
  );
}

function toolbar(x: ExplorerState, host: string): HTMLElement {
  const back = x.back[x.back.length - 1];
  const forward = x.forward[x.forward.length - 1];
  const above = parentPlace(x.place);
  return h(
    "div",
    { class: "xp-bar", role: "toolbar", "aria-label": "Go to another place" },
    tool("xp-back", "back", back ? `Back to ${placeWords(back)}` : "Back", {
      disabled: back === undefined,
      keys: "Alt+ArrowLeft",
    }),
    tool("xp-forward", "forward", forward ? `Forward to ${placeWords(forward)}` : "Forward", {
      disabled: forward === undefined,
      keys: "Alt+ArrowRight",
    }),
    tool("xp-up", "up", above ? `Up to ${placeWords(above)}` : "Up", {
      disabled: above === null,
      keys: "Backspace",
    }),
    tool("xp-refresh", "restart", "Refresh", { keys: "F5" }),
    x.editing === undefined ? pathBar(x, host) : pathField(x, host),
    h(
      "div",
      { class: "seg xp-views", role: "group", "aria-label": "View" },
      (["details", "list"] as const).map((view) =>
        h(
          "button",
          {
            type: "button",
            class: view === x.view ? "on" : null,
            "aria-pressed": view === x.view ? "true" : "false",
            "data-action": "xp-view",
            "data-arg": view,
          },
          view === "details" ? "Details" : "List",
        ),
      ),
    ),
  );
}

/** The path as parts to click: the computer, the drive, each folder. */
function pathBar(x: ExplorerState, host: string): HTMLElement {
  const parts = crumbs(x.place);
  const here = (last: boolean): string | null => (last ? "location" : null);
  return h(
    "nav",
    { class: "xp-path", "aria-label": `Place on ${host}` },
    h(
      "ol",
      null,
      h(
        "li",
        null,
        h(
          "button",
          {
            type: "button",
            class: "xp-crumb",
            "data-action": "xp-drives",
            "aria-current": here(x.place?.kind === "drives"),
            "aria-label": `The drives of ${host}`,
          },
          icon("desktop", "sm"),
          host,
        ),
      ),
      parts.map((part, index) =>
        h(
          "li",
          { "data-key": `c-${placeText(part.place)}` },
          h("span", { class: "xp-sep", "aria-hidden": "true" }, "›"),
          h(
            "button",
            {
              type: "button",
              class: "xp-crumb",
              "data-action": "xp-go",
              "data-arg": placeText(part.place),
              "aria-current": here(index === parts.length - 1),
            },
            part.label,
          ),
        ),
      ),
    ),
    h(
      "button",
      {
        type: "button",
        class: "btn sm ghost xp-tool xp-edit",
        id: "xp-edit",
        "data-action": "xp-edit",
        "aria-label": "Type a path",
        title: "Type a path",
        "aria-keyshortcuts": "Control+L",
      },
      icon("pencil", "sm"),
    ),
  );
}

/** The path as text to type: Enter goes there, Escape leaves the field. */
function pathField(x: ExplorerState, host: string): HTMLElement {
  return h(
    "div",
    { class: "xp-path edit" },
    h("label", { class: "sr-only", for: "xp-path-input" }, `Path on ${host}`),
    h("input", {
      type: "text",
      id: "xp-path-input",
      class: "xp-input mono",
      value: x.editing,
      placeholder: "C:\\Users",
      spellcheck: "false",
      autocomplete: "off",
      autocapitalize: "off",
      "aria-describedby": "xp-path-hint",
    }),
    h(
      "span",
      { class: "sr-only", id: "xp-path-hint" },
      "Press Enter to go there, Escape to leave the field.",
    ),
    button("Go", "xp-path-go", { variant: "sm primary" }),
    button("Cancel", "xp-path-cancel", { variant: "sm ghost" }),
  );
}

/** The refusal of a place, or the end of the session, as an assertive alert. */
function alertRegion(x: ExplorerState): HTMLElement {
  const region = h("div", { class: "alert xp-alert", role: "alert", id: "xp-alert" });
  if (x.ended !== undefined) {
    region.append(
      icon("alert"),
      h(
        "div",
        { class: "alert-body" },
        h("b", null, "Browsing has stopped"),
        h("p", null, x.ended),
      ),
    );
  } else if (x.refusal) {
    region.append(
      icon("alert"),
      h(
        "div",
        { class: "alert-body" },
        h("b", null, x.refusal.title),
        h("p", null, x.refusal.message),
        h("p", { class: "next" }, x.refusal.next),
      ),
      h(
        "button",
        { type: "button", class: "btn sm ghost", "data-action": "xp-dismiss" },
        "Dismiss",
      ),
    );
  }
  return region;
}

const COLUMNS: ReadonlyArray<{ key: SortKey; label: string; numeric?: boolean }> = [
  { key: "name", label: "Name" },
  { key: "modified", label: "Date modified" },
  { key: "type", label: "Type" },
  { key: "size", label: "Size", numeric: true },
];

const DRIVE_COLUMNS = ["Name", "Type", "Total size", "Free space"] as const;

/** The header of the Details view: a click on a column sorts by it. */
function header(x: ExplorerState): HTMLElement {
  const head = (index: number, attrs: Record<string, string | null>, content: Child): HTMLElement =>
    h(
      "div",
      { class: "xp-cell", role: "columnheader", "aria-colindex": index + 1, ...attrs },
      content,
    );
  return h(
    "div",
    { class: "xp-head", role: "row", "aria-rowindex": 1 },
    x.place?.kind === "drives"
      ? DRIVE_COLUMNS.map((label, index) =>
          head(index, { class: index > 1 ? "xp-cell num" : "xp-cell" }, label),
        )
      : COLUMNS.map((column, index) => {
          const sorted = x.sort.key === column.key;
          const direction = sorted ? (x.sort.descending ? "descending" : "ascending") : "none";
          return head(
            index,
            { "aria-sort": direction, class: column.numeric ? "xp-cell num" : "xp-cell" },
            h(
              "button",
              {
                type: "button",
                class: sorted ? "xp-sort on" : "xp-sort",
                "data-action": "xp-sort",
                "data-arg": column.key,
              },
              column.label,
              sorted
                ? h("span", { class: "xp-arrow", "aria-hidden": "true" }, x.sort.descending ? "▼" : "▲")
                : null,
            ),
          );
        }),
  );
}

const KIND_ICON: Record<ExplorerEntry["kind"], IconName> = {
  dir: "folder",
  file: "file",
  symlink: "link",
  other: "file",
};

function nameCell(iconName: IconName, name: string, kind: string): HTMLElement {
  return h(
    "div",
    { class: "xp-cell name", role: "gridcell", "aria-colindex": 1 },
    h("span", { class: `xp-icon ${kind}` }, icon(iconName, "sm")),
    h("span", { class: "xp-name", title: name }, name),
  );
}

function cell(index: number, text: string, numeric = false): HTMLElement {
  return h(
    "div",
    { class: numeric ? "xp-cell num" : "xp-cell", role: "gridcell", "aria-colindex": index },
    text,
  );
}

function entryCells(entry: ExplorerEntry, details: boolean): Child[] {
  const name = nameCell(KIND_ICON[entry.kind], entry.name, entry.kind);
  if (!details) return [name];
  return [
    name,
    cell(2, modifiedWords(entry.modifiedMs)),
    cell(3, typeWords(entry)),
    cell(4, sizeWords(entry), true),
  ];
}

/** A size the customer computer could not read is said to be unknown. */
function knownBytes(bytes: number | undefined): string {
  return bytes === undefined ? "Not known" : formatBytes(bytes);
}

function driveCells(drive: ExplorerDrive, details: boolean): Child[] {
  const name = nameCell("drive", driveName(drive), "drive");
  if (!details) return [name];
  return [
    name,
    cell(2, driveKindWords(drive.kind)),
    cell(3, knownBytes(drive.totalBytes), true),
    cell(4, knownBytes(drive.freeBytes), true),
  ];
}

/**
 * The rows in view, each at its place in a box as large as the whole list.
 * The row in focus is always drawn, so the grid can name it wherever the
 * list was scrolled to.
 */
export function explorerRows(x: ExplorerState): HTMLElement {
  const details = x.view === "details";
  const count = rowCount(x);
  const layout = layoutOf(x, count);
  const size = details
    ? `height:${layout.height}px`
    : `height:${layout.height}px;width:${layout.width}px`;
  const box = h("div", {
    class: "xp-rows",
    id: "xp-rows",
    role: "presentation",
    style: size,
  });
  const { first, last } = visibleRange(x);
  const focus = indexOfKey(x, x.focus);
  const indexes: number[] = [];
  if (focus >= 0 && focus < first) indexes.push(focus);
  for (let index = first; index < last; index += 1) indexes.push(index);
  if (focus >= last) indexes.push(focus);
  // The header is the first row of the Details view.
  const offset = details ? 2 : 1;
  for (const index of indexes) {
    const drive = x.place?.kind === "drives" ? x.drives[index] : undefined;
    const entry = x.place?.kind === "folder" ? x.entries[index] : undefined;
    const key = drive?.root ?? entry?.name;
    if (key === undefined) continue;
    const { left, top } = positionOf(x, index);
    const classes = ["xp-row"];
    if (x.selected.has(key)) classes.push("sel");
    if (index === focus) classes.push("focus");
    box.append(
      h(
        "div",
        {
          class: classes.join(" "),
          role: "row",
          id: `xp-row-${index}`,
          "aria-rowindex": index + offset,
          "aria-selected": x.selected.has(key) ? "true" : "false",
          "data-key": `r-${key}`,
          "data-row": key,
          style: details ? `top:${top}px` : `top:${top}px;left:${left}px`,
        },
        drive ? driveCells(drive, details) : entry ? entryCells(entry, details) : null,
      ),
    );
  }
  return box;
}

/** The grid: a box that scrolls, with the header and the rows in it. */
function grid(x: ExplorerState, host: string): HTMLElement {
  const details = x.view === "details";
  const count = rowCount(x);
  const focus = indexOfKey(x, x.focus);
  const busy = x.pending !== undefined || x.fresh !== undefined || x.content.state === "loading";
  return h(
    "div",
    {
      class: `xp-grid ${x.view}`,
      id: "xp-grid",
      role: "grid",
      tabindex: 0,
      "aria-label":
        x.place?.kind === "folder"
          ? `Files and folders in ${x.place.path}`
          : `Drives of ${host}`,
      "aria-rowcount": details ? count + 1 : count,
      "aria-colcount": details ? 4 : 1,
      "aria-multiselectable": "true",
      "aria-busy": busy ? "true" : "false",
      "aria-activedescendant": focus >= 0 ? `xp-row-${focus}` : null,
      "aria-describedby": "xp-status",
    },
    // The header lies in the box that scrolls and stays at its top, so its
    // columns are always as wide as those of the rows.
    h(
      "div",
      { class: "xp-scroll", id: "xp-scroll", role: "rowgroup" },
      details ? header(x) : null,
      explorerRows(x),
    ),
  );
}

/** What stands in the place of the rows when there are none, in words. */
function emptyState(x: ExplorerState, host: string): HTMLElement | null {
  const note = (...children: Child[]): HTMLElement =>
    h("div", { class: "xp-state", id: "xp-state" }, ...children);
  const spinner = (): HTMLElement => h("span", { class: "spinner", "aria-hidden": "true" });
  if (x.place === null) {
    if (x.pending) {
      return note(
        spinner(),
        h(
          "p",
          null,
          x.pending.place.kind === "drives"
            ? `Reading the drives of ${host}…`
            : `Opening ${x.pending.place.path}…`,
        ),
      );
    }
    return note(
      h(
        "p",
        null,
        x.ended !== undefined || x.refusal
          ? "Nothing is open."
          : `Look through the files of ${host}. The customer sees that you are browsing, and where.`,
      ),
      x.ended === undefined ? button("Show the drives", "xp-drives", { variant: "sm" }) : null,
    );
  }
  if (rowCount(x) > 0) return null;
  if (x.place.kind === "drives") {
    return note(h("p", null, `${host} lists no drives.`));
  }
  switch (x.content.state) {
    case "loading":
      return note(spinner(), h("p", null, "Loading…"));
    case "complete":
      return note(h("p", null, "This folder is empty."));
    case "limit":
    case "interrupted":
      // Said by the note under the list.
      return note(h("p", null, "Nothing was listed."));
  }
}

/** A list that is not the whole folder says so under it, with why. */
function incomplete(x: ExplorerState): HTMLElement | null {
  if (x.place?.kind !== "folder") return null;
  if (x.content.state === "limit") {
    return h(
      "div",
      { class: "note xp-note", role: "note", id: "xp-incomplete" },
      icon("alert"),
      h(
        "p",
        null,
        h("b", null, "This is not the whole folder. "),
        `It holds more than the ${rowCount(x)} entries CretLink lists. Open a subfolder, or use the terminal for the rest.`,
      ),
    );
  }
  if (x.content.state === "interrupted") {
    return h(
      "div",
      { class: "note xp-note", role: "note", id: "xp-incomplete" },
      icon("alert"),
      h("p", null, h("b", null, "This is not the whole folder. "), x.content.reason),
      x.ended === undefined ? button("Refresh", "xp-refresh", { variant: "sm" }) : null,
    );
  }
  return null;
}

/** The context menu: what the explorer can do with the rows, or the place. */
function menu(x: ExplorerState): HTMLElement | null {
  if (!x.menu) return null;
  const keys = x.menu.keys;
  const label =
    keys.length === 0
      ? `Actions for ${placeWords(x.place)}`
      : keys.length === 1
        ? `Actions for ${keys[0] ?? ""}`
        : `Actions for ${keys.length} items`;
  return h(
    "ul",
    {
      class: "xp-menu",
      id: "xp-menu",
      role: "menu",
      "aria-label": label,
      style: `left:${x.menu.x}px;top:${x.menu.y}px`,
    },
    menuItems(x).map((item) =>
      h(
        "li",
        { role: "none" },
        h(
          "button",
          {
            type: "button",
            role: "menuitem",
            "data-action": item.action,
            "aria-keyshortcuts": item.keys,
            disabled: item.disabled,
          },
          h("span", null, item.label),
          item.keys ? h("kbd", { "aria-hidden": "true" }, item.keys) : null,
        ),
      ),
    ),
  );
}

function fileEntries(s: TechnicianSnapshot): ActivityEntry[] {
  return s.activity.filter(
    (e) => e.entry === "item" && (e.item.kind === "file_sent" || e.item.kind === "file_received"),
  );
}

/** The files sent and received this session, newest first (ST-M009-008). */
function transfers(s: TechnicianSnapshot): HTMLElement {
  const files = fileEntries(s);
  const rows = h("ul", { class: "list files" });
  for (let i = files.length - 1; i >= 0; i -= 1) {
    const entry = files[i];
    if (!entry || entry.entry !== "item") continue;
    const item = entry.item;
    const sent = item.kind === "file_sent";
    rows.append(
      h(
        "li",
        { class: sent ? "it out" : "it", "data-key": `f-${item.id}` },
        h("span", { class: sent ? "k out" : "k" }, icon(sent ? "out" : "in")),
        h(
          "div",
          { class: "file-text" },
          h("b", { class: "mono" }, item.path ?? item.operation),
          h(
            "div",
            { class: "m" },
            sent
              ? h("span", { class: "flag" }, "Sent · left the customer computer")
              : h("span", { class: "who sys" }, "Received · arrived on the customer computer"),
            item.bytes !== undefined ? h("span", null, formatBytes(item.bytes)) : null,
            whoChip(item),
            resultBadge(item, s.now),
          ),
        ),
      ),
    );
  }
  return h(
    "section",
    { class: "xp-transfers", "aria-labelledby": "xp-transfers-h" },
    h("h3", { class: "xp-h", id: "xp-transfers-h" }, "Transfers"),
    files.length === 0
      ? h("p", { class: "muted" }, "No files were sent or received in this session.")
      : rows,
  );
}

/** The Files tab: the explorer, and under it the transfers of this session. */
export function explorerPanel(s: TechnicianSnapshot): HTMLElement {
  const x = s.explorer;
  const host = targetName(s);
  return h(
    "div",
    { class: "xp", id: "xp" },
    toolbar(x, host),
    alertRegion(x),
    h("div", { class: "xp-body" }, grid(x, host), emptyState(x, host)),
    incomplete(x),
    h("div", { class: "xp-status", id: "xp-status" }, statusWords(x)),
    menu(x),
    transfers(s),
  );
}
