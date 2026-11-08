// Microphone tab: the headset microphone test. Record opens the voice link
// of the classic stream (the core starts the stream again first when its
// stack has no voice), the bar shows the microphone's level as it records,
// Play sends the recording back to the headphones with its level on the same
// bar. The test ends when the tab closes (the core also ends one nobody asks
// about), and the stream goes back to what headset mode wants.

import { createSignal, For, onCleanup, Show } from "solid-js";
import { errorText, useApp } from "../../app/context";
import { Button } from "../../components/controls";
import { t } from "../../i18n";
import type { MicTestState } from "../../ipc/types";

/** How often the state is asked for while a test runs */
const POLL_MS = 70;
/** Segments of the level bar */
const SEGMENTS = 48;
/** The bar's floor in dBFS: quieter is dark */
const FLOOR_DB = -54;
/** How fast the bar falls back per poll (peak hold) */
const FALL = 0.82;

const IDLE: MicTestState = { phase: "idle", level: 0, recorded_ms: 0, max_ms: 10000, played_ms: 0, error: null };

/** A peak level 0..1 as the lit share of the bar (dB scale) */
export function barShare(level: number): number {
  if (level <= 0) return 0;
  const db = 20 * Math.log10(level);
  return Math.min(1, Math.max(0, (db - FLOOR_DB) / -FLOOR_DB));
}

const seconds = (ms: number) => `${(ms / 1000).toFixed(1)} s`;

export function MicPanel() {
  const { core } = useApp();
  const [st, setSt] = createSignal<MicTestState>(IDLE);
  const [bar, setBar] = createSignal(0);
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  let timer: number | undefined;

  const streaming = () => core.state.snap?.stream.state === "streaming";
  const phase = () => st().phase;
  const running = () => phase() !== "idle";

  const take = (s: MicTestState) => {
    setSt(s);
    if (s.error) setError(t(s.error));
    setBar((b) => Math.max(barShare(s.level), b * FALL));
    if (s.phase === "idle") stop();
    else if (timer === undefined) timer = window.setInterval(() => void poll(), POLL_MS);
  };
  const stop = () => {
    if (timer !== undefined) window.clearInterval(timer);
    timer = undefined;
    setBar(0);
  };
  const poll = async () => {
    try {
      take(await core.request("mic_test.status"));
    } catch {
      stop();
    }
  };
  const act = async (method: "mic_test.record" | "mic_test.stop" | "mic_test.play") => {
    setBusy(true);
    if (method === "mic_test.record") setError(null);
    try {
      take(await core.request(method));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  onCleanup(() => {
    const was = running();
    stop();
    if (was) void core.request("mic_test.end").catch(() => {});
  });

  const progress = () => {
    const s = st();
    switch (s.phase) {
      case "recording":
        return `${seconds(s.recorded_ms)} / ${seconds(s.max_ms)}`;
      case "playing":
        return `${seconds(s.played_ms)} / ${seconds(s.recorded_ms)}`;
      case "recorded":
        return seconds(s.recorded_ms);
      default:
        return "";
    }
  };
  const lit = () => Math.round(bar() * SEGMENTS);
  const playing = () => phase() === "playing";

  return (
    <div class="mic-test">
      <Show when={!streaming() && !running()}>
        <p class="muted">{t("quality.not_streaming")}</p>
      </Show>
      <div class="mic-test-row">
        <Show
          when={phase() === "recording" || phase() === "starting"}
          fallback={
            <Button kind="strong" icon="record" busy={busy()} disabled={!streaming()} onClick={() => void act("mic_test.record")}>
              {t("ui.mic_test.record")}
            </Button>
          }
        >
          <Button kind="strong" icon="stop" busy={phase() === "starting"} onClick={() => void act("mic_test.stop")}>
            {t("ui.mic_test.stop")}
          </Button>
        </Show>
        <Show
          when={playing()}
          fallback={
            <Button
              icon="play"
              disabled={busy() || !(phase() === "recorded" && st().recorded_ms > 0)}
              onClick={() => void act("mic_test.play")}
            >
              {t("ui.mic_test.play")}
            </Button>
          }
        >
          <Button icon="stop" onClick={() => void act("mic_test.stop")}>
            {t("ui.mic_test.stop")}
          </Button>
        </Show>
        <div
          class={`mic-meter ${playing() ? "playing" : ""}`}
          role="meter"
          aria-label={playing() ? t("ui.mic_test.play") : t("ui.headset_mic")}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(bar() * 100)}
        >
          <For each={Array.from({ length: SEGMENTS }, (_, i) => i)}>
            {(i) => <span class={i < lit() ? "on" : ""} />}
          </For>
        </div>
        <span class="mic-time num">{progress()}</span>
      </div>
      <p class="hint">{t("ui.mic_test.hint")}</p>
      <Show when={error()}>
        <p class="st-bad" role="alert">
          {error()}
        </p>
      </Show>
    </div>
  );
}
