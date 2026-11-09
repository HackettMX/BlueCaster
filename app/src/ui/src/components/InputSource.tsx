// A device's Input Source on Audio Devices (issue #86; it was on Codecs):
// the headphones' microphone rendered to an output of the PC, a virtual
// cable whose recording side apps use as a microphone. Off, Auto (only
// while an app records from it) or On, with the mode on the right of its
// label and the device below, always there (disabled while Off) so that
// switching does not move the page. The list names each device by the side
// apps pick ("CABLE Output", not "CABLE Input"; an output whose recording
// side another one already names, with its own name added); the 16-channel
// cable and outputs without a recording side (under Monitoring) only in
// developer mode. The output source's cable is listed but disabled, saying
// so. A classic device that has no profile yet shows what its first one
// gets: Auto with VB-CABLE (the core's default_mic_output).

import { createSignal, Show } from "solid-js";
import { useApp } from "../app/context";
import { t } from "../i18n";
import type { AudioDevice, Device, Profile } from "../ipc/types";
import {
  audioEntries,
  deviceKey,
  entryBase,
  HEADSET_MODES,
  headsetMode,
  multichannelCable,
  type HeadsetMode,
  vbCable,
} from "../lib/profile";
import { saveHeadsetMic } from "../screens/Codecs";
import { Segmented, Select, type Option } from "./controls";
import { Icon } from "./Icon";

/** The render endpoint the stream records with this source (the default
 *  output for System Loopback): the microphone must not go there */
function sourceDevice(
  source: Pick<Profile, "capture_mode" | "audio_device_id">,
  devices: readonly AudioDevice[],
) {
  if (source.capture_mode === "virtual" || source.capture_mode === "output")
    return source.audio_device_id;
  if (source.capture_mode === "loopback")
    return devices.find((d) => d.is_default)?.id ?? "";
  return "";
}

export function InputSource(props: {
  device: Device;
  devices: readonly AudioDevice[];
  /** The device's output source as shown (a loop is refused) */
  source: Pick<Profile, "capture_mode" | "audio_device_id">;
}) {
  const app = useApp();
  const snap = () => app.core.state.snap!;
  const entry = () =>
    audioEntries(snap()).find((e) => e.key === deviceKey(props.device)) ?? null;
  const developer = () => snap().settings.developer_mode;
  const looped = () => sourceDevice(props.source, props.devices);
  /** The profile shown: a classic device without one, its first one's defaults */
  const shown = (): Profile | null => {
    const e = entry();
    if (!e) return null;
    if (e.profile) return e.profile;
    const base = entryBase(e, snap().profiles);
    const cable = vbCable(props.devices);
    return !e.le_audio && cable && cable.id !== looped()
      ? {
          ...base,
          headset_mic: "auto",
          mic_device_id: cable.id,
          mic_device_name: cable.name,
        }
      : base;
  };
  const mode = () => (shown() ? headsetMode(shown()!) : "off");
  const chosen = () => shown()?.mic_device_id ?? "";
  /** The output source and what records with it (the other side of the
   *  same cable): the microphone would reach the headphones or be mixed
   *  with what plays, so these are listed but cannot be chosen */
  const usedBySource = (id: string) => {
    const source = props.devices.find((d) => d.id === looped());
    if (!source || !id) return false;
    const d = props.devices.find((x) => x.id === id);
    return (
      id === source.id ||
      (!!d?.capture_name && d.capture_name === source.capture_name)
    );
  };
  const [busy, setBusy] = createSignal(false);
  const save = async (patch: Partial<Profile>) => {
    const e = entry();
    const p = shown();
    if (!e || !p) return;
    setBusy(true);
    // The shown device goes with the first change (a device without a profile)
    await saveHeadsetMic(app, e, {
      mic_device_id: p.mic_device_id,
      mic_device_name: p.mic_device_name,
      ...patch,
    });
    setBusy(false);
  };
  const options = (): Option<string>[] => {
    // Cables first (the standard one before the 16-channel one), then what
    // only plays the microphone; a group's options next to each other
    const rank = (d: AudioDevice) =>
      (d.capture_name || d.is_virtual ? 0 : 2) + Number(multichannelCable(d));
    const list = props.devices
      .filter(
        (d) =>
          d.id === chosen() ||
          developer() ||
          (d.is_virtual && d.capture_name && !multichannelCable(d)),
      )
      .sort((a, b) => rank(a) - rank(b));
    // Outputs sharing a recording side ("CABLE Input" and "CABLE In 16ch" both
    // record as "CABLE Output"): only the first keeps that name, the others
    // add their own (the 16-channel one is after, so the standard cable keeps it)
    const label = (d: AudioDevice) => {
      if (!d.capture_name) return d.name;
      const first = list.find((x) => x.capture_name === d.capture_name);
      return first === d
        ? d.capture_name
        : `${d.capture_name} · ${d.name.replace(/\s*\(.*\)$/, "")}`;
    };
    const out: Option<string>[] = list.map((d) => ({
      value: d.id,
      label: usedBySource(d.id)
        ? t("capture.mic_used_by_source", label(d))
        : label(d),
      disabled: usedBySource(d.id),
      // Developer mode lists every output: those with no recording side only
      // play the microphone (issue #86: they read as microphones)
      group: developer()
        ? d.capture_name || d.is_virtual
          ? t("capture.virtual")
          : t("capture.mic_monitoring")
        : undefined,
    }));
    const p = shown();
    // One chosen before and not there now: by its name
    if (p?.mic_device_id && !list.some((d) => d.id === p.mic_device_id)) {
      out.unshift({
        value: p.mic_device_id,
        label: t(
          "capture.volume_sync_missing",
          p.mic_device_name || p.mic_device_id,
        ),
      });
    }
    if (!p?.mic_device_id)
      out.unshift({ value: "", label: t("ui.codec.none"), disabled: true });
    return out;
  };
  const choose = (id: string) => {
    const d = props.devices.find((x) => x.id === id);
    if (!d || id === chosen()) return;
    void save({ mic_device_id: d.id, mic_device_name: d.name });
  };
  return (
    <Show when={shown()}>
      <div class="input-source">
        <div class="row input-source-head">
          <span class="row-label">{t("capture.input_source")}</span>
          <Segmented<HeadsetMode>
            label={t("capture.input_source")}
            value={mode()}
            options={HEADSET_MODES.map((m) => ({
              value: m,
              label: t(`ui.headset_mic_${m}`),
              disabled: busy(),
            }))}
            onChange={(m) => void save({ headset_mic: m, mic_muted: false })}
          />
        </div>
        <div class="inline capture-source">
          <Icon name="mic" />
          <Select
            label={t("ui.mic_output")}
            value={chosen()}
            options={options()}
            disabled={busy() || mode() === "off"}
            onChange={choose}
          />
        </div>
        <Show when={!entry()?.le_audio}>
          <p class="hint">{t("ui.headset_mic_quality")}</p>
        </Show>
        <Show when={mode() !== "off" && !chosen()}>
          <p class="field-warn">{t("ui.headset_mic_no_output")}</p>
        </Show>
        <Show when={mode() !== "off" && usedBySource(chosen())}>
          <p class="field-warn">
            <Icon name="warning" />
            <span>{t("error.headset_mic_loop")}</span>
          </p>
        </Show>
      </div>
    </Show>
  );
}
