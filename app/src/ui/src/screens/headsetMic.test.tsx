// Headset microphone (not only in developer mode): Off / Auto / On and the
// device under each device's Input Source on Audio Devices (issue #86; it
// was on Codecs), saved with the device's profile, the mute chip on Home's
// card and in the flyout (an icon, with the voice codec and why the
// microphone did not open in its tooltip), and the voice codec in the
// sidebar while a voice link runs

import { cleanup, fireEvent, render, screen, waitFor, within } from "@solidjs/testing-library";
import { afterEach, describe, expect, it } from "vitest";
import { App } from "../app/App";
import { setLanguage, t } from "../i18n";
import { MockCore, type MockScenario } from "../ipc/mock";
import type { Profile } from "../ipc/types";
import { createCore } from "../ipc/store";
import { endSettle } from "./Codecs";

let mock: MockCore | null = null;

function start(hash: string, scenario: MockScenario = "normal", window?: "flyout") {
  location.hash = hash;
  const calls: { method: string; params: unknown }[] = [];
  mock = new MockCore({ scenario, speed: 0, ...(window ? { window } : {}), onCall: (method, params) => calls.push({ method, params }) });
  const core = createCore(mock);
  const view = render(() => <App core={core} />);
  const saves = () =>
    calls.filter((c) => c.method === "profile.save").map((c) => (c.params as { profile: Profile }).profile);
  return { calls, core, view, saves };
}

afterEach(async () => {
  cleanup();
  endSettle();
  mock?.dispose();
  mock = null;
  await setLanguage("en");
});

const CABLE = "{0.0.0.00000000}.{cable}";
const VOICEMEETER = "{0.0.0.00000000}.{voicemeeter}";
/** A device's entry on Audio Devices */
const deviceEntry = (name: RegExp) =>
  waitFor(() => {
    const entry = [...document.querySelectorAll<HTMLElement>("main .device-entry")].find((e) => name.test(e.textContent ?? ""));
    expect(entry).toBeTruthy();
    return entry!;
  });
const micModes = (entry: HTMLElement) => within(entry).queryByRole("radiogroup", { name: t("capture.input_source") });
const mode = (entry: HTMLElement, m: "off" | "auto" | "on") =>
  within(micModes(entry)!).getByRole("radio", { name: t(`ui.headset_mic_${m}`) });
const micSelect = (entry: HTMLElement) =>
  within(entry).getByRole("combobox", { name: t("ui.mic_output") }) as HTMLSelectElement;
const MUTE = () => t("ui.headset_mic_mute");
const UNMUTE = () => t("ui.headset_mic_unmute");

describe("Input Source on Audio Devices", () => {
  it("is on every device outside developer mode too, and no longer on Codecs", async () => {
    const { core } = start("#/devices");
    const wh = await deviceEntry(/WH-1000XM5/);
    expect(core.state.snap?.settings.developer_mode).toBe(false);
    await waitFor(() => expect(mode(wh, "off")).toBeChecked());
    location.hash = "#/codecs";
    const card = await screen.findByRole("region", { name: "WH-1000XM5" });
    expect(within(card).queryByRole("radiogroup", { name: t("ui.headset_mic") })).toBeNull();
  });

  it("names each device by the side apps record from, without the 16-channel cable", async () => {
    start("#/devices", "voice");
    const wh = await deviceEntry(/WH-1000XM5/);
    await waitFor(() =>
      expect([...micSelect(wh).options].map((o) => o.textContent)).toEqual([
        "CABLE Output (VB-Audio Virtual Cable)",
        "Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)",
      ]),
    );
    expect(micSelect(wh).value).toBe(CABLE);
  });

  it("names the 16-channel cable by its own name in developer mode, not a second CABLE Output", async () => {
    const { core } = start("#/devices", "voice");
    const wh = await deviceEntry(/WH-1000XM5/);
    await core.request("settings.set", { settings: { ...core.state.snap!.settings, developer_mode: true } });
    await waitFor(() => {
      const labels = [...micSelect(wh).options].map((o) => o.textContent);
      expect(labels).toContain("CABLE In 16ch (VB-Audio Virtual Cable)");
      expect(labels.filter((l) => l === "CABLE Output (VB-Audio Virtual Cable)")).toHaveLength(1);
    });
    const output = [...micSelect(wh).options].find((o) => o.textContent === "CABLE Output (VB-Audio Virtual Cable)");
    expect(output?.value).toBe(CABLE);
  });

  it("saves the mode and the device with the device's profile", async () => {
    const { saves } = start("#/devices", "voice");
    const xb = await deviceEntry(/SRS-XB13/);
    await waitFor(() => expect(mode(xb, "off")).toBeChecked());
    // There while Off too, so that switching moves nothing (issue #86)
    expect(micSelect(xb)).toBeDisabled();
    expect(within(xb).getByText(new RegExp(t("ui.headset_mic_quality").replace(/[.]/g, "\\.")))).toBeInTheDocument();
    fireEvent.click(mode(xb, "on"));
    await waitFor(() => expect(saves().at(-1)).toMatchObject({ device_name: "SRS-XB13", headset_mic: "on", mic_device_id: "" }));
    await within(xb).findByText(t("ui.headset_mic_no_output"));
    await waitFor(() => expect(micSelect(xb)).toBeEnabled());
    const select = micSelect(xb);
    select.selectedIndex = [...select.options].findIndex((o) => o.value === VOICEMEETER);
    fireEvent.change(select);
    await waitFor(() =>
      expect(saves().at(-1)).toMatchObject({
        device_name: "SRS-XB13",
        headset_mic: "on",
        mic_device_id: VOICEMEETER,
        mic_device_name: "Voicemeeter Input (VB-Audio Voicemeeter VAIO)",
      }),
    );
    await waitFor(() => expect(within(xb).queryByText(t("ui.headset_mic_no_output"))).toBeNull());
  });

  it("keeps the device for Off, disabled", async () => {
    const { saves } = start("#/devices", "voice");
    const wh = await deviceEntry(/WH-1000XM5/);
    await waitFor(() => expect(mode(wh, "on")).toBeChecked());
    fireEvent.click(mode(wh, "auto"));
    await waitFor(() => expect(saves().at(-1)).toMatchObject({ device_name: "WH-1000XM5", headset_mic: "auto", mic_device_id: CABLE }));
    fireEvent.click(mode(wh, "off"));
    await waitFor(() => expect(saves().at(-1)).toMatchObject({ headset_mic: "off", mic_device_id: CABLE }));
    await waitFor(() => expect(micSelect(wh)).toBeDisabled());
    expect(micSelect(wh).value).toBe(CABLE);
  });

  it("refuses the device the output source plays from", async () => {
    start("#/devices", "voice");
    const wh = await deviceEntry(/WH-1000XM5/);
    await waitFor(() => expect(micSelect(wh).value).toBe(CABLE));
    const source = within(wh).getByLabelText(t("capture.mode")) as HTMLSelectElement;
    await waitFor(() => expect([...source.options].some((o) => o.value === `virtual:${CABLE}`)).toBe(true));
    fireEvent.change(source, { target: { value: `virtual:${CABLE}` } });
    await waitFor(() => expect([...micSelect(wh).options].find((o) => o.value === CABLE)?.disabled).toBe(true));
    await within(wh).findByText(t("error.headset_mic_loop"));
  });

  it("warns of call quality only for a classic device", async () => {
    start("#/devices", "voice");
    const wh = await deviceEntry(/WH-1000XM5/);
    await waitFor(() => expect(mode(wh, "on")).toBeChecked());
    expect(within(wh).getByText(new RegExp(t("ui.headset_mic_quality").replace(/[.]/g, "\\.")))).toBeInTheDocument();
    const le = [...document.querySelectorAll<HTMLElement>("main .device-entry")].find(
      (e) => /Galaxy Buds2 Pro/.test(e.textContent ?? "") && micModes(e),
    );
    if (le) expect(within(le).queryByText(new RegExp(t("ui.headset_mic_quality").replace(/[.]/g, "\\.")))).toBeNull();
  });
});

describe("status", () => {
  const homeCard = (name: string) => screen.findByRole("group", { name });

  it("shows the voice codec in the sidebar and on the chip, not on Home's card", async () => {
    const { core } = start("#/home", "voice");
    const wh = await homeCard("WH-1000XM5");
    await core.request("stream.start", { profile: "WH-1000XM5 (LDAC)" });
    await waitFor(() => expect(document.querySelector(".status-pill .voice-codec")).toHaveTextContent("Voice: mSBC"));
    expect(document.querySelector(".status-pill .voice-codec")).toHaveClass("st-warn");
    // The card keeps its lines as the microphone opens and closes (issue #86)
    expect(wh.querySelector(".voice-codec")).toBeNull();
    await waitFor(() => expect(within(wh).getByRole("button", { name: MUTE() })).toHaveAttribute("title", `${MUTE()} (mSBC)`));
  });

  it("drops the voice codec when headset mode is turned off while streaming", async () => {
    const { core } = start("#/devices", "voice");
    await core.request("stream.start", { profile: "WH-1000XM5 (LDAC)" });
    await waitFor(() => expect(core.state.snap?.stream.voice?.codec).toBe("msbc"));
    const wh = await deviceEntry(/WH-1000XM5/);
    await waitFor(() => expect(mode(wh, "on")).toBeChecked());
    fireEvent.click(mode(wh, "off"));
    await waitFor(() => expect(core.state.snap?.stream.voice ?? null).toBeNull());
    expect(core.state.snap?.stream.state).toBe("streaming");
  });

  it("measures the call link on Diagnostics as it measures A2DP, with the microphone's packets", async () => {
    const { core } = start("#/diagnostics", "voice");
    await core.request("stream.start", { profile: "WH-1000XM5 (LDAC)" });
    await waitFor(() => expect(core.state.snap?.stream.voice?.codec).toBe("msbc"));
    const row = (label: string) => screen.getByText(label, { selector: "dt" }).closest(".lq-row")!;
    await waitFor(() => expect(row(t("quality.codec"))).toHaveTextContent(t("ui.diag.voice_codec_value", "mSBC", 16)));
    // The SCO payloads sent, not 0
    await waitFor(() => expect(row(t("quality.bitrate"))).toHaveTextContent(t("quality.bitrate_value", 64)));
    const mic = await screen.findByRole("heading", { name: t("ui.diag.mic_title") });
    await waitFor(() => expect(row(t("ui.diag.mic_received"))).not.toHaveTextContent(/^\D*0$/));
    expect(mic.closest("section")).toHaveTextContent(t("ui.diag.mic_damaged"));
  });

  it("shows LE Audio's voice codec without the warning", async () => {
    const { core } = start("#/home", "voice");
    await homeCard("Galaxy Buds2 Pro");
    await core.request("stream.start", { profile: "Buds (LE Audio)" });
    const voice = await waitFor(() => {
      const v = document.querySelector(".status-pill .voice-codec");
      expect(v).not.toBeNull();
      return v!;
    });
    expect(voice).toHaveTextContent("Voice: LC3");
    expect(voice).not.toHaveClass("st-warn");
  });
});

describe("headset chip", () => {
  const homeCard = (name: string) => screen.findByRole("group", { name });

  it("is not on Home with headset mode off", async () => {
    start("#/home");
    const wh = await homeCard("WH-1000XM5");
    expect(wh.querySelector(".headset-chip")).toBeNull();
  });

  it("is only on the cards of devices with a microphone output", async () => {
    start("#/home", "voice");
    const wh = await homeCard("WH-1000XM5");
    await within(wh).findByRole("button", { name: MUTE() });
    const xb = await homeCard("SRS-XB13");
    expect(xb.querySelector(".headset-chip")).toBeNull();
  });

  it("mutes and unmutes on Home, an icon alone, keeping the mode", async () => {
    const { saves } = start("#/home", "voice");
    const wh = await homeCard("WH-1000XM5");
    const on = await within(wh).findByRole("button", { name: MUTE() });
    expect(on).toHaveTextContent("");
    expect(on).toHaveClass("on");
    fireEvent.click(on);
    await waitFor(() =>
      expect(saves().at(-1)).toMatchObject({ device_name: "WH-1000XM5", headset_mic: "on", mic_muted: true }),
    );
    const muted = await within(wh).findByRole("button", { name: UNMUTE() });
    expect(muted).toHaveClass("muted");
    expect(muted).not.toHaveClass("on");
    // Each click once the last save is done
    await waitFor(() => expect(muted).toBeEnabled());
    fireEvent.click(muted);
    await waitFor(() => expect(saves().at(-1)).toMatchObject({ headset_mic: "on", mic_muted: false }));
    await within(wh).findByRole("button", { name: MUTE() });
  });

  it("shows Auto waiting with no app recording, and is gone for Off", async () => {
    start("#/devices", "voice");
    const entry = await deviceEntry(/WH-1000XM5/);
    await waitFor(() => expect(mode(entry, "on")).toBeChecked());
    fireEvent.click(mode(entry, "auto"));
    await waitFor(() => expect(mode(entry, "auto")).toBeChecked());
    location.hash = "#/home";
    const wh = await homeCard("WH-1000XM5");
    const auto = await within(wh).findByRole("button", { name: MUTE() });
    expect(auto).toHaveClass("waiting");
    location.hash = "#/devices";
    const again = await deviceEntry(/WH-1000XM5/);
    await waitFor(() => expect(mode(again, "auto")).toBeChecked());
    fireEvent.click(mode(again, "off"));
    await waitFor(() => expect(mode(again, "off")).toBeChecked());
    location.hash = "#/home";
    const card = await homeCard("WH-1000XM5");
    await waitFor(() => expect(card.querySelector(".headset-chip")).toBeNull());
  });

  it("shows why the microphone did not open on the chip, not as the card's status", async () => {
    const { core } = start("#/home", "voice");
    const wh = await homeCard("WH-1000XM5");
    await core.request("stream.start", { profile: "WH-1000XM5 (LDAC)" });
    await waitFor(() => expect(core.state.snap?.stream.state).toBe("streaming"));
    mock!.noteStream("error.headset_mic_loop");
    await waitFor(() => expect(wh.querySelector(".headset-chip")).toHaveClass("warn"));
    expect(wh.querySelector(".headset-chip")).toHaveAttribute("title", t("error.headset_mic_loop"));
    expect(within(wh).getByRole("status")).toHaveTextContent(t("status.connected"));
    expect(wh).not.toHaveTextContent(t("error.headset_mic_loop"));
  });

  it("closes the voice link while muted and unmutes from Audio Devices", async () => {
    const { core, saves } = start("#/home", "voice");
    const wh = await homeCard("WH-1000XM5");
    await core.request("stream.start", { profile: "WH-1000XM5 (LDAC)" });
    await waitFor(() => expect(core.state.snap?.stream.voice?.codec).toBe("msbc"));
    const chip = await within(wh).findByRole("button", { name: MUTE() });
    await waitFor(() => expect(chip).toHaveClass("linked"));
    fireEvent.click(chip);
    await waitFor(() => expect(core.state.snap?.stream.voice ?? null).toBeNull());
    expect(core.state.snap?.stream.state).toBe("streaming");
    // Choosing a mode on Audio Devices unmutes
    location.hash = "#/devices";
    const entry = await deviceEntry(/WH-1000XM5/);
    await waitFor(() => expect(micModes(entry)).not.toBeNull());
    fireEvent.click(mode(entry, "auto"));
    await waitFor(() => expect(saves().at(-1)).toMatchObject({ headset_mic: "auto", mic_muted: false }));
  });

  it("is in the flyout and saves from there", async () => {
    const { core, saves } = start("", "voice", "flyout");
    await core.request("stream.start", { profile: "WH-1000XM5 (LDAC)" });
    const chip = await screen.findByRole("button", { name: MUTE() });
    expect(chip.closest(".fly-state")).not.toBeNull();
    fireEvent.click(chip);
    await waitFor(() => expect(saves().at(-1)).toMatchObject({ device_name: "WH-1000XM5", mic_muted: true }));
    await screen.findByRole("button", { name: UNMUTE() });
  });

  it("is not in the flyout with headset mode off", async () => {
    const { core } = start("", "normal", "flyout");
    await core.request("stream.start", { profile: "WH-1000XM5 (LDAC)" });
    await waitFor(() => expect(core.state.snap?.stream.state).not.toBe("idle"));
    expect(document.querySelector(".headset-chip")).toBeNull();
  });
});
