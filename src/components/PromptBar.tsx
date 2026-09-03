import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  AvailableCommand,
  AvailableModelInfo,
  ManagedAgentInfo,
  SessionMode,
  TimelineItem,
} from "../types";
import { SESSION_MODE_OPTIONS } from "../types";
import { usePromptHistoryBrowse } from "../hooks/usePromptHistoryBrowse";
import { filterSlashCommands } from "../utils/slashCommands";
import {
  applySessionModeToPrompt,
  cycleSessionMode,
} from "../utils/sessionMode";
import { HistoryMenu } from "./HistoryMenu";
import { PromptChipSelect } from "./PromptChipSelect";
import {
  resolveReasoningOptions,
  selectedReasoningEffort,
} from "../utils/reasoningEffort";

/** Composer send outcome. `accepted: false` keeps the draft in the box. */
export type SendResult = { accepted: boolean; hint?: string };

interface Props {
  managed: ManagedAgentInfo | null;
  busy: boolean;
  /**
   * Grok session mode (single control).
   * Shift+Tab: Normal → Plan → Auto → Always-approve (docs.x.ai).
   */
  sessionMode: SessionMode;
  onSessionModeChange: (mode: SessionMode) => void;
  onSend: (text: string) => void | Promise<void | SendResult>;
  /** Send was refused; parent paints the Disconnected-style turn strip. */
  onRefusal?: (hint: string | null) => void;
  /** Agent-advertised slash commands (ACP available_commands_update). */
  availableCommands?: AvailableCommand[];
  /**
   * Current session timeline — Up on empty composer opens history
   * (Grok Build-style list overlay).
   */
  timelineItems?: TimelineItem[];
  /** Reset history browse / local sent when the active session changes. */
  sessionId?: string | null;
  /** Current model id (e.g. grok-4.6), shown left of Send. */
  modelId?: string | null;
  /**
   * Agent-advertised catalog (`availableModels`). Empty while non-blocking
   * startup has not yet filled the list — falls back to a built-in set.
   */
  availableModels?: AvailableModelInfo[];
  /** Change the session model or reasoning level via ACP set_session_model. */
  onModelChange?: (modelId: string, reasoningEffort?: string) => void;
  /** Resolved thinking level for the chip. */
  reasoningEffort?: string | null;

  /** Show Stop when the managed agent process can be terminated. */
  canStop?: boolean;
  onStop?: () => void;
}

export function PromptBar({
  managed,
  busy,
  sessionMode,
  onSessionModeChange,
  onSend,
  onRefusal,
  availableCommands = [],
  timelineItems = [],
  sessionId = null,
  modelId = null,
  availableModels = [],
  onModelChange,
  reasoningEffort = null,
  canStop = false,
  onStop,
}: Props) {
  const [text, setText] = useState("");
  const [menuIndex, setMenuIndex] = useState(0);
  const [menuOpen, setMenuOpen] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  /**
   * After insert / Esc, keep the menu closed until the user types again.
   * Prevents the "slashQuery truthy → always reopen" loop that blocked Enter-to-send.
   */
  const suppressMenuRef = useRef(false);

  const connected =
    managed && managed.status !== "stopped" && managed.status !== "error";
  const running =
    managed?.status === "running" || managed?.status === "stopping";
  const stopping = managed?.status === "stopping";
  const awaiting = managed?.status === "awaitingPermission";
  const trimmedText = text.trim();
  // Local slashes work offline; first agent message auto-connects ACP.
  const canSend = Boolean(trimmedText && !busy && !stopping);

  const modeMeta =
    SESSION_MODE_OPTIONS.find((o) => o.value === sessionMode) ??
    SESSION_MODE_OPTIONS[0]!;

  const focusEnd = useCallback((value: string) => {
    requestAnimationFrame(() => {
      const el = textareaRef.current;
      if (!el) return;
      el.focus();
      const pos = value.length;
      el.selectionStart = pos;
      el.selectionEnd = pos;
    });
  }, []);

  const onHistoryExclusive = useCallback(() => {
    suppressMenuRef.current = true;
    setMenuOpen(false);
  }, []);

  const onSessionReset = useCallback(() => {
    setMenuOpen(false);
    suppressMenuRef.current = false;
    onRefusal?.(null);
  }, [onRefusal]);

  // Session switch: hook owns history + composer text; we only reset slash chrome.
  const history = usePromptHistoryBrowse({
    sessionId,
    timelineItems,
    text,
    setText,
    focusEnd,
    onHistoryExclusive,
    onSessionReset,
  });

  const slashQuery = useMemo(() => parseSlashQuery(text), [text]);

  const filteredCommands = useMemo(() => {
    if (!slashQuery) return [] as AvailableCommand[];
    return filterSlashCommands(availableCommands, slashQuery.query);
  }, [slashQuery, availableCommands]);

  // While browsing history, keep slash menu closed (Grok history intercept).
  const showMenu =
    !history.active &&
    menuOpen &&
    Boolean(!stopping && !busy) &&
    Boolean(slashQuery) &&
    filteredCommands.length > 0;

  useEffect(() => {
    setMenuIndex(0);
  }, [slashQuery?.query, filteredCommands.length]);

  useEffect(() => {
    if (!slashQuery) {
      setMenuOpen(false);
      suppressMenuRef.current = false;
    }
  }, [slashQuery]);

  useEffect(() => {
    if (!showMenu) return;
    const el = menuRef.current?.querySelector<HTMLElement>(
      `[data-cmd-idx="${menuIndex}"]`,
    );
    el?.scrollIntoView({ block: "nearest" });
  }, [menuIndex, showMenu]);

  function applyCommand(cmd: AvailableCommand) {
    const needsInput = Boolean(cmd.inputHint);
    const next = needsInput ? `/${cmd.name} ` : `/${cmd.name}`;
    history.detach();
    suppressMenuRef.current = true;
    setMenuOpen(false);
    setText(next);
    focusEnd(next);
  }

  async function sendIfReady() {
    const trimmed = text.trim();
    if (!trimmed || stopping || busy) return;
    // First non-local message auto-connects ACP in App.handleSend.
    const payload = applySessionModeToPrompt(sessionMode, trimmed);
    setMenuOpen(false);
    suppressMenuRef.current = false;
    const result = await Promise.resolve(onSend(payload));
    if (result && result.accepted === false) {
      onRefusal?.(result.hint ?? "");
      return;
    }
    // Record wire text (matches timeline user cards after mode prefix).
    history.recordSent(payload);
    history.detach();
    onRefusal?.(null);
    setText("");
  }

  /** True when composer already holds the fully-applied form of `cmd`. */
  function isCompleteCommand(cmd: AvailableCommand): boolean {
    const trimmed = text.trimEnd();
    if (cmd.inputHint) {
      return (
        trimmed === `/${cmd.name}` ||
        trimmed.startsWith(`/${cmd.name} `) ||
        trimmed.startsWith(`/${cmd.name}\t`)
      );
    }
    return trimmed === `/${cmd.name}`;
  }

  return (
    <div className="prompt-bar">
      <div className="prompt-composer">
        {history.active && history.selected != null && (
          <HistoryMenu
            entries={history.entries}
            selected={history.selected}
            listRef={history.listRef}
            onSelect={history.select}
            onAccept={history.accept}
          />
        )}
        {showMenu && (
          <div
            className="slash-menu"
            ref={menuRef}
            role="listbox"
            aria-label="Slash commands"
          >
            <div className="slash-menu-hint muted small">
              Grok slash commands · tab insert · enter send when complete · esc
              close
            </div>
            {filteredCommands.map((cmd, i) => (
              <button
                key={cmd.name}
                type="button"
                role="option"
                data-cmd-idx={i}
                aria-selected={i === menuIndex}
                className={`slash-menu-item${i === menuIndex ? " active" : ""}`}
                onMouseEnter={() => setMenuIndex(i)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  applyCommand(cmd);
                }}
              >
                <span className="slash-menu-name">/{cmd.name}</span>
                <span className="slash-menu-desc">{cmd.description}</span>
                {cmd.inputHint && (
                  <span className="slash-menu-hint-arg muted">
                    {cmd.inputHint}
                  </span>
                )}
              </button>
            ))}
          </div>
        )}
        <textarea
          ref={textareaRef}
          className="prompt-input"
          rows={2}
          placeholder={
            connected
              ? awaiting
                ? "Approve or deny the permission request above to continue…"
                : running
                  ? "Agent is working… enter adds this message to the queue"
                  : sessionMode === "plan"
                    ? "Plan mode · next free-text send becomes /plan … · shift+tab to cycle"
                    : "Message the agent… / for commands · ↑ history · enter send · shift+tab mode"
              : "Message the agent… first send connects · ↑ history when empty"
          }
          value={text}
          disabled={stopping || busy}
          onChange={(e) => {
            const next = e.target.value;
            // Typing while browsing detaches (keep populated text).
            if (history.active) history.detach();
            suppressMenuRef.current = false;
            onRefusal?.(null);
            setText(next);
            const q = parseSlashQuery(next);
            if (q && !q.hasArgs) setMenuOpen(true);
            else setMenuOpen(false);
          }}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing) return;

            // Shift+Tab: Grok session mode cycle.
            if (e.key === "Tab" && e.shiftKey) {
              e.preventDefault();
              e.stopPropagation();
              if (!running && !awaiting) {
                onSessionModeChange(cycleSessionMode(sessionMode));
              }
              const el = textareaRef.current;
              requestAnimationFrame(() => {
                el?.focus({ preventScroll: true });
              });
              return;
            }

            // History panel first (modal over slash, matching Grok).
            if (history.handleKey(e)) return;

            if (showMenu) {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setMenuIndex((i) =>
                  Math.min(filteredCommands.length - 1, i + 1),
                );
                return;
              }
              if (e.key === "ArrowUp") {
                e.preventDefault();
                setMenuIndex((i) => Math.max(0, i - 1));
                return;
              }
              if (e.key === "Escape") {
                e.preventDefault();
                suppressMenuRef.current = true;
                setMenuOpen(false);
                return;
              }
              if (e.key === "Tab") {
                e.preventDefault();
                const cmd = filteredCommands[menuIndex];
                if (cmd) applyCommand(cmd);
                return;
              }
              if (e.key === "Enter" && !e.ctrlKey && !e.metaKey) {
                const cmd = filteredCommands[menuIndex];
                if (slashQuery && !slashQuery.query) {
                  // Bare `/` — fall through to send.
                } else if (cmd && slashQuery && !slashQuery.hasArgs) {
                  if (isCompleteCommand(cmd)) {
                    e.preventDefault();
                    suppressMenuRef.current = true;
                    setMenuOpen(false);
                    sendIfReady();
                    return;
                  }
                  e.preventDefault();
                  applyCommand(cmd);
                  return;
                }
              }
            }

            if (e.key !== "Enter") return;

            if (e.ctrlKey || e.metaKey) {
              e.preventDefault();
              history.detach();
              const el = e.currentTarget;
              const start = el.selectionStart ?? text.length;
              const end = el.selectionEnd ?? text.length;
              const next = `${text.slice(0, start)}\n${text.slice(end)}`;
              suppressMenuRef.current = false;
              setText(next);
              setMenuOpen(false);
              requestAnimationFrame(() => {
                const pos = start + 1;
                el.selectionStart = pos;
                el.selectionEnd = pos;
              });
              return;
            }

            e.preventDefault();
            sendIfReady();
          }}
        />
        <div className="prompt-toolbar">
          <div className="prompt-controls">
            <PromptChipSelect
              label="Mode"
              value={sessionMode}
              displayLabel={modeMeta.label}
              tooltip={`${modeMeta.hint} · Press Shift+Tab to cycle modes`}
              accent={modeMeta.accent}
              glyph={modeGlyph(sessionMode)}
              options={SESSION_MODE_OPTIONS}
              disabled={busy || running || awaiting}
              onChange={onSessionModeChange}
            />
            {modelId && (
              <>
                <ModelSelector
                  current={modelId}
                  availableModels={availableModels}
                  onChange={onModelChange}
                />
                {onModelChange && (
                  <ReasoningLevelSelector
                    modelId={modelId}
                    availableModels={availableModels}
                    reasoningEffort={reasoningEffort}
                    busy={busy}
                    running={Boolean(running || awaiting)}
                    onChange={(effort) => onModelChange(modelId, effort)}
                  />
                )}
              </>
            )}
          </div>
          <div className="prompt-send-group">
            {canStop && onStop && (
              <button
                className="btn danger-btn prompt-stop"
                type="button"
                disabled={busy || stopping}
                title="Stop the agent process for this task"
                onClick={onStop}
              >
                Stop
              </button>
            )}
            <button
              className="btn primary prompt-send"
              type="button"
              disabled={!canSend}
              title="enter to send · ↑ empty for history · ctrl+enter newline · shift+tab mode"
              onClick={sendIfReady}
            >
              {running || awaiting ? "Queue" : "Send"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

function modeGlyph(mode: SessionMode): string {
  switch (mode) {
    case "plan":
      return "◇";
    case "auto":
      return "◎";
    case "alwaysApprove":
      return "⚡";
    default:
      return "○";
  }
}

/**
 * Detect `/command` or `/command args` at the start of the composer.
 * Closes once the user has typed past the first word and is editing free text
 * that no longer starts with `/`.
 */
function parseSlashQuery(
  text: string,
): { query: string; hasArgs: boolean } | null {
  if (!text.startsWith("/")) return null;
  if (text.includes("\n")) return null;
  const body = text.slice(1);
  const space = body.search(/\s/);
  if (space === -1) {
    return { query: body, hasArgs: false };
  }
  return { query: body.slice(0, space), hasArgs: true };
}

// ── Model selector ──────────────────────────────────────────────────────

/** Offline / empty-catalog fallback when agent has not yet pushed models. */
const FALLBACK_MODELS: { id: string; label: string }[] = [
  { id: "grok-4.6", label: "Grok 4.6" },
  { id: "grok-4.5", label: "Grok 4.5" },
  { id: "grok-4", label: "Grok 4" },
  { id: "grok-4-mini", label: "Grok 4 Mini" },
  { id: "grok-4-fast", label: "Grok 4 Fast" },
];

function resolveModelOptions(
  available: AvailableModelInfo[],
  current: string,
): { id: string; label: string }[] {
  const fromAgent = available
    .map((m) => {
      const id = m.modelId.trim();
      if (!id) return null;
      const label = (m.name ?? "").trim() || id;
      return { id, label };
    })
    .filter((m): m is { id: string; label: string } => m != null);

  const list = fromAgent.length > 0 ? fromAgent : [...FALLBACK_MODELS];
  if (current && !list.some((m) => m.id === current)) {
    list.unshift({ id: current, label: current });
  }
  return list;
}

function ModelSelector({
  current,
  availableModels = [],
  onChange,
}: {
  current: string;
  availableModels?: AvailableModelInfo[];
  onChange?: (modelId: string, reasoningEffort?: string) => void;
}) {
  const options = resolveModelOptions(availableModels, current);
  const currentOption = options.find((m) => m.id === current) ?? options[0];
  const displayLabel = currentOption?.label ?? current;
  const tooltip =
    availableModels.length === 0
      ? `Current model: ${current} (catalog loading…)`
      : `Current model: ${current}`;

  if (!onChange || options.length <= 1) {
    return (
      <div className="prompt-chip accent-neutral">
        <span
          className="prompt-chip-trigger prompt-chip-static"
          title={tooltip}
          aria-label={`Model: ${displayLabel}`}
        >
          <span className="prompt-chip-glyph" aria-hidden>
            ✦
          </span>
          <span className="prompt-chip-value">{displayLabel}</span>
        </span>
      </div>
    );
  }

  return (
    <PromptChipSelect
      label="Model"
      value={current}
      displayLabel={displayLabel}
      tooltip={tooltip}
      accent="neutral"
      glyph="✦"
      options={options.map((m) => ({
        value: m.id,
        label: m.label,
        hint: m.id,
      }))}
      onChange={onChange}
    />
  );
}

function ReasoningLevelSelector({
  modelId,
  availableModels,
  reasoningEffort,
  busy,
  running,
  onChange,
}: {
  modelId: string;
  availableModels: AvailableModelInfo[];
  reasoningEffort?: string | null;
  busy: boolean;
  running: boolean;
  onChange: (reasoningEffort: string) => void;
}) {
  const options = resolveReasoningOptions(modelId, availableModels);
  const active = selectedReasoningEffort(options, reasoningEffort);
  if (!active) return null;

  return (
    <PromptChipSelect
      label="Thinking"
      value={active.value}
      displayLabel={active.label}
      tooltip={active.description ?? `Thinking level: ${active.label}`}
      accent="neutral"
      glyph="◈"
      options={options.map((option) => ({
        value: option.value,
        label: option.label,
        hint: option.description ?? "Grok thinking level",
      }))}
      disabled={busy || running}
      onChange={onChange}
    />
  );
}

