// Taking over the pairings Windows' Bluetooth made with the adapter BlueCaster
// now has on WinUSB (issue #86): in the setup's device step only. Shown only
// while Windows paired an audio device through this adapter that BlueCaster
// has not saved (other radios' pairings are no use here, and Windows may
// have no radio running at all). Its button lists those devices, all ticked;
// the core reads the keys of the ticked ones after an administrator prompt
// and saves them with Windows' names.

import { createResource, createSignal, For, Show } from "solid-js";
import { useApp } from "../app/context";
import { t } from "../i18n";
import { Button, Dialog } from "./controls";
import { Icon } from "./Icon";

export function ImportPairings() {
  const app = useApp();
  const { core } = app;
  const snap = () => core.state.snap!;
  const adapter = () => snap().adapter.adapters.find((a) => a.driver === "winusb" && a.address)?.address ?? null;
  // What Windows paired through this adapter (no prompt to read the list),
  // against the saved ones
  const [windows, { refetch }] = createResource(adapter, (address) =>
    core.request("device.windows_paired_through", { adapter: address }).catch(() => []),
  );
  const unsaved = () => {
    const saved = snap().devices.filter((d) => d.saved).map((d) => d.address.toUpperCase());
    return (windows() ?? []).filter((d) => !saved.includes(d.address.toUpperCase()));
  };
  const [open, setOpen] = createSignal(false);
  // The addresses left unticked in the list
  const [skipped, setSkipped] = createSignal<string[]>([]);
  const ticked = () => unsaved().filter((d) => !skipped().includes(d.address));
  const tick = (address: string, on: boolean) =>
    setSkipped((list) => (on ? list.filter((a) => a !== address) : [...list, address]));
  const [busy, setBusy] = createSignal(false);
  const importNow = (address: string) => {
    const chosen = ticked().map((d) => d.address);
    if (chosen.length === 0 || busy()) return;
    setBusy(true);
    void app
      .run(async () => {
        const r = await core.request("device.import_windows_pairings", { adapter: address, addresses: chosen });
        if (!r.adapter_found) return app.notify(t("ui.adapter.import_none"));
        const kept = r.kept.length > 0 ? ` ${t("ui.adapter.import_kept", r.kept.length)}` : "";
        app.notify(`${t("ui.adapter.import_done", r.imported.length)}${kept}`);
        void refetch();
      })
      .finally(() => {
        setBusy(false);
        setOpen(false);
      });
  };
  return (
    <Show when={adapter() && unsaved().length > 0 ? adapter() : null}>
      {(address) => (
        <div class="row import-pairings">
          <div class="row-text">
            <span class="row-label">{t("ui.adapter.import_windows")}</span>
            <span class="hint">{t("ui.devices.import_hint")}</span>
          </div>
          <Button
            onClick={() => {
              setSkipped([]);
              setOpen(true);
            }}
          >
            {t("ui.devices.import_one")}
          </Button>
          <Dialog
            open={open()}
            title={t("ui.adapter.import_windows")}
            confirm={t("ui.devices.import_one")}
            confirmIcon="shield"
            cancel={t("modal.cancel")}
            busy={busy()}
            onConfirm={() => importNow(address())}
            onCancel={() => setOpen(false)}
          >
            <ul class="import-list">
              <For each={unsaved()}>
                {(d) => (
                  <li>
                    <label class="check-item">
                      <input
                        type="checkbox"
                        checked={!skipped().includes(d.address)}
                        disabled={busy()}
                        onChange={(e) => tick(d.address, e.currentTarget.checked)}
                      />
                      <Icon name="headphones" />
                      <span class="check-item-text">
                        <span class="ellipsis">{d.name || d.address}</span>
                        <Show when={d.name}>
                          <span class="mono muted">{d.address}</span>
                        </Show>
                      </span>
                    </label>
                  </li>
                )}
              </For>
            </ul>
          </Dialog>
        </div>
      )}
    </Show>
  );
}
