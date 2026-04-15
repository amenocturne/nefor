/**
 * Statusline — custom status bar rendered as a widget
 *
 * Replaces Pi's native extension status line (space-joined, no separators)
 * with a clean widget that shows segments separated by | with color support.
 *
 * Other extensions register segments via setSegment/removeSegment
 * instead of calling ctx.ui.setStatus() directly.
 *
 * State lives on globalThis so all module instances share it
 * (Pi loads extensions with separate import() calls).
 */

import type { ExtensionAPI, ExtensionContext } from "@mariozechner/pi-coding-agent";
import { Text, truncateToWidth } from "@mariozechner/pi-tui";

type Segment = {
  text: string;
  order: number;
  color?: string;
};

type StatuslineState = {
  segments: Map<string, Segment>;
  ctx: ExtensionContext | null;
};

const G = globalThis as any;
if (!G.__statusline) {
  G.__statusline = {
    segments: new Map<string, Segment>(),
    ctx: null,
  } satisfies StatuslineState;
}
const state: StatuslineState = G.__statusline;

export function setSegment(
  key: string,
  text: string,
  opts?: { order?: number; color?: string },
): void {
  state.segments.set(key, {
    text,
    order: opts?.order ?? 50,
    color: opts?.color,
  });
  refresh();
}

export function removeSegment(key: string): void {
  state.segments.delete(key);
  refresh();
}

function refresh(): void {
  if (!state.ctx) return;

  if (state.segments.size === 0) {
    state.ctx.ui.setWidget("statusline", undefined);
    return;
  }

  state.ctx.ui.setWidget(
    "statusline",
    (_tui, theme) => {
      const text = new Text("", 0, 0);
      return {
        render(width: number): string[] {
          const sorted = [...state.segments.values()]
            .filter((s) => s.text)
            .sort((a, b) => a.order - b.order);
          if (sorted.length === 0) return [];

          const parts = sorted.map((seg) =>
            seg.color ? theme.fg(seg.color, seg.text) : seg.text,
          );
          const line = parts.join(theme.fg("dim", " | "));
          text.setText(truncateToWidth(line, width));
          return text.render(width);
        },
        invalidate() {
          text.invalidate();
        },
      };
    },
    { placement: "belowEditor" },
  );
}

export default function (pi: ExtensionAPI) {
  pi.on("session_start", async (_event, extCtx) => {
    state.ctx = extCtx;
    refresh();
  });
}
