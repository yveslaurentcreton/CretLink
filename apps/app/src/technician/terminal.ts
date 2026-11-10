// The embedded interactive terminal of the live session (ST-M008-001, kept by
// ST-M009-008).
//
// The console hosts the session backbone (ST-M004-007), so the terminal
// attaches in-process: `open_terminal` opens a remote pty on the consented
// session and streams raw VT bytes back on the `"console-terminal"` event.
// xterm renders them; keystrokes go back as raw UTF-8 bytes so the *remote*
// pty owns the echo; the panel never echoes locally and never fabricates a
// shell. Every backend command fails closed with `no_consented_session`
// outside full control.
//
// The live view renders two empty `data-keep` slots (the status badge and the
// terminal body); the controller hands them to `attach()` after each render.
// `morph()` leaves the slots alone, so one xterm instance lives across tab
// switches and re-renders. It is torn down only when full control ends
// (`dispose()`), which closes the remote pty (fail-closed, no residue into the
// next session).

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { h } from "../shared/dom";
import { CONSOLE_TERMINAL_EVENT, type TerminalFrame, api, isCommandError } from "./api";

type TerminalStatus = "connecting" | "live" | "closed" | "revoked" | "unavailable";

const BADGE: Record<TerminalStatus, { cls: string; label: string }> = {
  connecting: { cls: "warn", label: "connecting…" },
  live: { cls: "", label: "live" },
  closed: { cls: "bad", label: "shell closed" },
  revoked: { cls: "bad", label: "revoked" },
  unavailable: { cls: "bad", label: "unavailable" },
};

export class ConsoleTerminal {
  private term: Terminal | null = null;
  private fit: FitAddon | null = null;
  private host: HTMLDivElement | null = null;
  private badge: HTMLSpanElement | null = null;
  private termId: number | null = null;
  private starting = false;
  private ended = false;
  private status: TerminalStatus = "connecting";
  private unlisten: UnlistenFn | null = null;
  // Bumped by `dispose()`, so a shell that opens after its session ended is
  // closed instead of adopted.
  private generation = 0;
  private readonly onWindowResize = (): void => this.refit();

  /**
   * Puts the terminal into the slots of the current render and, when the
   * Terminal tab is visible, opens xterm and (once) the remote shell.
   */
  attach(badgeSlot: HTMLElement, bodySlot: HTMLElement, visible: boolean): void {
    if (!this.host) {
      this.host = h("div", { class: "sh-xterm" });
    }
    if (!this.badge) {
      this.badge = h("span", { class: "sh-badge" });
      this.paintBadge();
    }
    if (this.host.parentElement !== bodySlot) bodySlot.replaceChildren(this.host);
    if (this.badge.parentElement !== badgeSlot) badgeSlot.replaceChildren(this.badge);
    if (!visible || !this.host.isConnected) return;
    if (!this.term) {
      const term = new Terminal({
        convertEol: false,
        cursorBlink: true,
        fontFamily: "'Cascadia Code', 'Consolas', ui-monospace, monospace",
        fontSize: 12.5,
        scrollback: 2000,
        theme: { background: "#0b1120", foreground: "#cbd5e1", cursor: "#86efac" },
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(this.host);
      term.onData((data) => this.sendInput(data));
      term.onResize(({ cols, rows }) => this.sendResize(cols, rows));
      this.term = term;
      this.fit = fit;
      window.addEventListener("resize", this.onWindowResize);
    }
    this.refit();
    if (this.termId === null && !this.starting && !this.ended) {
      void this.start();
    }
  }

  refit(): void {
    if (this.term && this.host?.isConnected && this.host.offsetParent !== null) {
      try {
        this.fit?.fit();
      } catch {
        // A zero-size container (mid-transition) throws; the next attach refits.
      }
    }
  }

  private async start(): Promise<void> {
    const generation = this.generation;
    this.starting = true;
    this.setStatus("connecting");
    try {
      const cols = this.term?.cols ?? 80;
      const rows = this.term?.rows ?? 24;
      const info = await api.openTerminal(cols, rows);
      if (generation !== this.generation) {
        void api.termClose(info.term).catch(() => undefined);
        return;
      }
      this.termId = info.term;
      // Subscribe before syncing size so no early output is missed; frames are
      // filtered by our own `term` id, so a stale terminal's frames are ignored.
      const unlisten = await listen<TerminalFrame>(CONSOLE_TERMINAL_EVENT, (event) => {
        this.onFrame(event.payload);
      });
      if (generation !== this.generation) {
        unlisten();
        return;
      }
      this.unlisten = unlisten;
      this.setStatus("live");
      // Sync the pty to the fitted window so full-screen TUIs render correctly.
      this.sendResize(cols, rows);
    } catch (error) {
      if (generation !== this.generation) return;
      this.ended = true;
      this.setStatus("unavailable");
      this.term?.writeln(`\r\n\x1b[31m[terminal unavailable: ${describeErr(error)}]\x1b[0m`);
    } finally {
      if (generation === this.generation) this.starting = false;
    }
  }

  private onFrame(frame: TerminalFrame): void {
    if (this.termId === null || frame.term !== this.termId) return;
    switch (frame.kind) {
      case "output":
        if (frame.data && frame.data.length > 0) {
          this.term?.write(new Uint8Array(frame.data));
        }
        break;
      case "exited": {
        const code = frame.exitCode != null ? ` (exit ${frame.exitCode})` : "";
        const how = frame.outcome ? ` ${frame.outcome}` : "";
        this.term?.writeln(`\r\n\x1b[90m[shell ended${how}${code}]\x1b[0m`);
        this.finalize("closed");
        break;
      }
      case "revoked":
        this.term?.writeln("\r\n\x1b[31m[session revoked — terminal closed]\x1b[0m");
        this.finalize("revoked");
        break;
      case "dropped":
        this.term?.writeln(
          "\r\n\x1b[33m[output dropped — consumer overran; terminal detached]\x1b[0m",
        );
        this.finalize("closed");
        break;
    }
  }

  private sendInput(data: string): void {
    if (this.termId === null) return;
    const bytes = Array.from(new TextEncoder().encode(data));
    void api.termInput(this.termId, bytes).catch((error: unknown) => this.onCommandFailure(error));
  }

  private sendResize(cols: number, rows: number): void {
    if (this.termId === null) return;
    void api
      .termResize(this.termId, cols, rows)
      .catch((error: unknown) => this.onCommandFailure(error));
  }

  // A rejected input/resize means the session dropped under us (fail-closed):
  // stop treating the shell as live so no further keystrokes are attempted.
  private onCommandFailure(error: unknown): void {
    if (isCommandError(error) && error.stage === "no_consented_session") {
      this.finalize("revoked");
    }
  }

  private finalize(status: TerminalStatus): void {
    this.termId = null;
    this.ended = true;
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }
    this.setStatus(status);
  }

  private setStatus(status: TerminalStatus): void {
    this.status = status;
    this.paintBadge();
  }

  private paintBadge(): void {
    const badge = this.badge;
    if (!badge) return;
    const spec = BADGE[this.status];
    badge.className = `sh-badge${spec.cls ? ` ${spec.cls}` : ""}`;
    badge.replaceChildren(
      h("span", { class: "pd", "aria-hidden": "true" }),
      h("span", null, `Terminal ${spec.label}`),
    );
  }

  /** Full control ended: close the remote pty and drop every reference. */
  dispose(): void {
    this.generation += 1;
    const id = this.termId;
    this.termId = null;
    this.ended = false;
    this.starting = false;
    this.status = "connecting";
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }
    if (id !== null) {
      void api.termClose(id).catch(() => {
        // A pty that already exited maps to an honest error we can drop here.
      });
    }
    if (this.term) {
      this.term.dispose();
      this.term = null;
      window.removeEventListener("resize", this.onWindowResize);
    }
    this.fit = null;
    this.host = null;
    this.badge = null;
  }
}

function describeErr(error: unknown): string {
  if (isCommandError(error)) return `${error.stage}: ${error.message}`;
  if (error instanceof Error) return error.message;
  return "unknown error";
}
