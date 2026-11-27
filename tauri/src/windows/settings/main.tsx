import "../../App.css";
import React, { useEffect, useRef, useState } from "react";
import ReactDOM from "react-dom/client";
import clsx from "clsx";
import { HiCog6Tooth, HiEyeSlash, HiMicrophone, HiVideoCamera } from "react-icons/hi2";
import { LuCommand, LuScreenShare } from "react-icons/lu";
import { useDisableNativeContextMenu, useSystemTheme } from "@/lib/hooks";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import {
  typedInvoke,
  type AppVeilApplication,
  type InstalledApplication,
  type ScreenSharePickerMode,
  type ScreenShareResolution,
} from "@/core_payloads";
import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { tauriUtils } from "@/windows/window-utils";
import { OS, URLS } from "@/constants";
import posthog from "posthog-js";
import useStore from "@/store/store";

const queryClient = new QueryClient();

const screenShareResolutionItems = [
  { id: "P1080", title: "1080p (lowest latency)" },
  { id: "P1440", title: "1440p (balanced)" },
  { id: "P4K", title: "4K (maximum resolution)" },
] satisfies Array<{ id: ScreenShareResolution; title: string }>;

const screenSharePickerModeItems = [
  { id: "Screen", title: "Entire display" },
  { id: "Window", title: "Single window" },
] satisfies Array<{ id: ScreenSharePickerMode; title: string }>;

document.body.classList.add("settings-body");

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <SettingsWindow />
    </QueryClientProvider>
  </React.StrictMode>,
);

/** A pane's centred header: a coloured icon tile, a title and one line on what it covers. */
function PaneHeader({
  icon,
  tileClassName,
  title,
  subtitle,
}: {
  icon: React.ReactNode;
  tileClassName: string;
  title: string;
  subtitle: string;
}) {
  return (
    <header className="flex flex-col items-center text-center mb-8">
      <div
        className={clsx(
          "size-16 rounded-2xl flex items-center justify-center text-white shadow-md [&>svg]:size-9",
          tileClassName,
        )}
      >
        {icon}
      </div>
      <h1 className="mt-3 text-xl font-semibold tracking-normal text-gray-900 lg:text-xl dark:text-zinc-100">
        {title}
      </h1>
      <p className="mt-1 max-w-xs text-sm leading-snug text-gray-500 dark:text-zinc-400">{subtitle}</p>
    </header>
  );
}

/** A titled card of setting rows, separated by hairlines. */
function SettingsGroup({ title, children }: { title?: string; children: React.ReactNode }) {
  return (
    <section className="flex flex-col gap-2">
      {title && (
        <h2 className="border-0 px-1 pb-0 text-[13px] font-semibold tracking-normal text-gray-700 dark:text-zinc-300">
          {title}
        </h2>
      )}
      <div className="rounded-xl bg-white px-4 ring-1 ring-black/5 divide-y divide-gray-200 dark:bg-white/[0.04] dark:ring-white/5 dark:divide-white/[0.07]">
        {children}
      </div>
    </section>
  );
}

/** One setting: its name and an optional explanation on the left, its control on the right. */
function SettingRow({
  title,
  description,
  htmlFor,
  children,
}: {
  title: string;
  description?: React.ReactNode;
  htmlFor?: string;
  children?: React.ReactNode;
}) {
  return (
    <div className="flex min-h-12 items-center justify-between gap-4 py-2.5">
      <div className="flex min-w-0 flex-col">
        <label htmlFor={htmlFor} className="text-[13px] text-gray-900 dark:text-zinc-100">
          {title}
        </label>
        {description && (
          <span className="mt-0.5 text-xs leading-snug text-gray-500 dark:text-zinc-400">{description}</span>
        )}
      </div>
      {children && <div className="shrink-0">{children}</div>}
    </div>
  );
}

function ToggleRow({
  title,
  description,
  checked,
  onCheckedChange,
}: {
  title: string;
  description?: React.ReactNode;
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
}) {
  return (
    <SettingRow title={title} description={description}>
      <Switch aria-label={title} checked={checked} onCheckedChange={onCheckedChange} />
    </SettingRow>
  );
}

const rowSelectClassName =
  "h-8 w-[220px] rounded-md border-gray-300 bg-white text-[13px] shadow-none dark:border-zinc-600 dark:bg-zinc-800";

function SelectRow<T extends string>({
  id,
  title,
  description,
  value,
  items,
  onValueChange,
}: {
  id: string;
  title: string;
  description?: React.ReactNode;
  value: T;
  items: Array<{ id: T; title: string }>;
  onValueChange: (value: T) => void;
}) {
  return (
    <SettingRow title={title} description={description} htmlFor={id}>
      <Select
        value={value}
        onValueChange={(next) => {
          if (next !== value) onValueChange(next as T);
        }}
      >
        <SelectTrigger id={id} className={rowSelectClassName}>
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {items.map((item) => (
            <SelectItem key={item.id} value={item.id}>
              {item.title}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </SettingRow>
  );
}

function ShortcutRow({
  title,
  description,
  value,
  onCommit,
}: {
  title: string;
  description: string;
  value: string;
  onCommit: (accel: string) => void;
}) {
  const [recording, setRecording] = useState(false);
  const listenerRef = useRef<((e: KeyboardEvent) => void) | null>(null);

  const startRecording = () => {
    setRecording(true);
    const handler = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();

      if (e.key === "Escape" && !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) {
        stopRecording();
        return;
      }

      if (["Meta", "Control", "Alt", "Shift"].includes(e.key)) return;

      const hasModifier = e.metaKey || e.ctrlKey || e.altKey;
      if (!hasModifier) return;

      const parts: string[] = [];
      if (OS === "macos") {
        if (e.metaKey) parts.push("Cmd");
        if (e.ctrlKey) parts.push("Ctrl");
        if (e.altKey) parts.push("Alt");
        if (e.shiftKey) parts.push("Shift");
      } else {
        if (e.ctrlKey) parts.push("Ctrl");
        if (e.altKey) parts.push("Alt");
        if (e.shiftKey) parts.push("Shift");
      }
      parts.push(e.key.toUpperCase());
      const accel = parts.join("+");

      stopRecording();
      onCommit(accel);
    };

    listenerRef.current = handler;
    window.addEventListener("keydown", handler, true);
  };

  const stopRecording = () => {
    setRecording(false);
    if (listenerRef.current) {
      window.removeEventListener("keydown", listenerRef.current, true);
      listenerRef.current = null;
    }
  };

  return (
    <SettingRow title={title} description={description}>
      <button
        type="button"
        className={clsx(
          "h-7 min-w-[96px] rounded-md px-3 text-xs font-medium transition-colors",
          recording ? "bg-blue-500 text-white" : (
            "bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-zinc-700 dark:text-zinc-200 dark:hover:bg-zinc-600"
          ),
        )}
        onClick={() => (recording ? stopRecording() : startRecording())}
      >
        {recording ?
          "Press keys…"
        : value ?
          <span className="font-mono">{value}</span>
        : "Record"}
      </button>
    </SettingRow>
  );
}

function applicationIcon(application: InstalledApplication): string | undefined {
  if (!application.icon_png) return undefined;
  let binary = "";
  for (const byte of application.icon_png) binary += String.fromCharCode(byte);
  return `data:image/png;base64,${btoa(binary)}`;
}

function AppVeilSettings({
  rows,
  installedApplications,
  onChange,
}: {
  rows: AppVeilApplication[];
  installedApplications: InstalledApplication[];
  onChange: (rows: AppVeilApplication[]) => Promise<void>;
}) {
  const [query, setQuery] = useState("");
  const [highlighted, setHighlighted] = useState(0);
  const [saving, setSaving] = useState(false);
  const addedBundleIds = new Set(rows.map((row) => row.bundle_id));
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const results =
    normalizedQuery ?
      installedApplications
        .filter((application) => application.name.toLocaleLowerCase().includes(normalizedQuery))
        .slice(0, 8)
    : [];
  const nameCounts = installedApplications.reduce<Record<string, number>>((counts, application) => {
    counts[application.name] = (counts[application.name] ?? 0) + 1;
    return counts;
  }, {});
  const commit = async (nextRows: AppVeilApplication[]) => {
    if (saving) return;
    setSaving(true);
    try {
      await onChange(nextRows);
    } catch (error) {
      console.error("Failed to update App Veil settings", error);
    } finally {
      setSaving(false);
    }
  };

  const add = async (application: InstalledApplication) => {
    if (addedBundleIds.has(application.bundle_id)) return;
    await commit([...rows, { bundle_id: application.bundle_id, enabled: true }]);
    setQuery("");
    setHighlighted(0);
  };

  return (
    <SettingsGroup>
      <SettingRow
        title="Hide these apps when you share your screen"
        description="Viewers don't see their windows. You still see and use them normally."
      />
      {rows.length === 0 ?
        <div className="py-4 text-[13px] text-gray-500 dark:text-zinc-400">No applications are hidden yet.</div>
      : rows.map((row) => {
          const application = installedApplications.find((candidate) => candidate.bundle_id === row.bundle_id);
          const name = application?.name ?? "Application not found";
          const icon = application && applicationIcon(application);
          return (
            <div key={row.bundle_id} className="group flex min-h-12 items-center gap-3 py-2">
              {icon ?
                <img src={icon} alt="" className="size-7 shrink-0" />
              : <div aria-hidden className="size-7 shrink-0 rounded bg-gray-100 dark:bg-zinc-700" />}
              <span className="min-w-0 flex-1">
                <span className="block truncate text-[13px] text-gray-900 dark:text-zinc-100">{name}</span>
                {!application && (
                  <span className="block truncate text-xs text-gray-500 dark:text-zinc-400">{row.bundle_id}</span>
                )}
              </span>
              <Switch
                aria-label={`Hide ${application?.name ?? row.bundle_id} from viewers`}
                checked={row.enabled}
                disabled={!application || saving}
                onCheckedChange={(enabled) =>
                  void commit(
                    rows.map((candidate) =>
                      candidate.bundle_id === row.bundle_id ? { ...candidate, enabled } : candidate,
                    ),
                  )
                }
              />
              <button
                type="button"
                aria-label={`Remove ${application?.name ?? row.bundle_id} from App Veil`}
                disabled={saving}
                className="size-7 rounded text-lg text-gray-500 opacity-0 hover:bg-gray-100 focus:opacity-100 focus:outline-none focus:ring-2 focus:ring-gray-400 group-hover:opacity-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
                onClick={() => void commit(rows.filter((candidate) => candidate.bundle_id !== row.bundle_id))}
              >
                −
              </button>
            </div>
          );
        })
      }
      <div className="relative py-3">
        <Input
          aria-label="Add application to App Veil"
          role="combobox"
          aria-expanded={results.length > 0}
          aria-controls="app-veil-results"
          aria-activedescendant={results[highlighted] ? `app-veil-result-${highlighted}` : undefined}
          placeholder="Add an application…"
          value={query}
          disabled={saving}
          className="h-8 text-[13px] dark:bg-zinc-900/40"
          onChange={(event) => {
            setQuery(event.target.value);
            setHighlighted(0);
          }}
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              setQuery("");
              setHighlighted(0);
            } else if (event.key === "ArrowDown" && results.length) {
              event.preventDefault();
              setHighlighted((index) => (index + 1) % results.length);
            } else if (event.key === "ArrowUp" && results.length) {
              event.preventDefault();
              setHighlighted((index) => (index + results.length - 1) % results.length);
            } else if (event.key === "Enter" && results[highlighted]) {
              event.preventDefault();
              const selected = results[highlighted];
              if (selected) void add(selected);
            }
          }}
        />
        {normalizedQuery && (
          <div
            id="app-veil-results"
            role="listbox"
            className="absolute z-20 mt-1 max-h-56 w-full overflow-y-auto rounded-md border border-gray-200 bg-white p-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
          >
            {results.length === 0 ?
              <div className="px-2 py-3 text-sm text-gray-500 dark:text-zinc-400">No installed applications found.</div>
            : results.map((application, index) => {
                const added = addedBundleIds.has(application.bundle_id);
                const icon = applicationIcon(application);
                return (
                  <button
                    id={`app-veil-result-${index}`}
                    role="option"
                    aria-selected={index === highlighted}
                    key={application.bundle_id}
                    disabled={added || saving}
                    className={clsx(
                      "flex w-full items-center gap-2 rounded px-2 py-2 text-left text-sm disabled:opacity-60",
                      index === highlighted && "bg-gray-100 dark:bg-zinc-700",
                    )}
                    onMouseEnter={() => setHighlighted(index)}
                    onClick={() => void add(application)}
                  >
                    {icon && <img src={icon} alt="" className="size-7 shrink-0" />}
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-gray-700 dark:text-zinc-200">{application.name}</span>
                      {(nameCounts[application.name] ?? 0) > 1 && (
                        <span className="block truncate text-xs text-gray-500">{application.bundle_id}</span>
                      )}
                    </span>
                    {added && <span className="text-xs text-gray-500">Added</span>}
                  </button>
                );
              })
            }
          </div>
        )}
      </div>
    </SettingsGroup>
  );
}

type SectionId = "general" | "audio" | "webcam" | "screen-share" | "app-veil" | "hotkeys";

const SECTIONS: {
  id: SectionId;
  title: string;
  subtitle: string;
  icon: React.ReactNode;
  tileClassName: string;
  macOnly?: boolean;
}[] = [
  {
    id: "general",
    title: "General",
    subtitle: "How Hopp behaves, updates and connects",
    icon: <HiCog6Tooth />,
    tileClassName: "bg-gradient-to-b from-slate-400 to-slate-500",
  },
  {
    id: "audio",
    title: "Audio",
    subtitle: "Your microphone and how you sound in calls",
    icon: <HiMicrophone />,
    tileClassName: "bg-gradient-to-b from-orange-400 to-orange-500",
  },
  {
    id: "webcam",
    title: "Webcam",
    subtitle: "Your camera and when it turns on",
    icon: <HiVideoCamera />,
    tileClassName: "bg-gradient-to-b from-green-400 to-green-500",
  },
  {
    id: "screen-share",
    title: "Screen Share",
    subtitle: "What you share and how pairs interact with your screen",
    icon: <LuScreenShare />,
    tileClassName: "bg-gradient-to-b from-violet-400 to-violet-500",
  },
  {
    id: "app-veil",
    title: "App Veil",
    subtitle: "Hide apps from your pairs while you share",
    icon: <HiEyeSlash />,
    tileClassName: "bg-gradient-to-b from-red-400 to-red-500",
    macOnly: true,
  },
  {
    id: "hotkeys",
    title: "Hotkeys",
    subtitle: "Shortcuts for muting, hanging up and more during a call",
    icon: <LuCommand />,
    tileClassName: "bg-gradient-to-b from-cyan-400 to-cyan-500",
  },
];

function SettingsWindow() {
  useDisableNativeContextMenu();
  useSystemTheme();

  const [serverUrl, setServerUrl] = useState("");
  const [section, setSection] = useState<SectionId>("general");
  const [savingDefaultMic, setSavingDefaultMic] = useState(false);
  const [defaultMicError, setDefaultMicError] = useState("");

  const { data: settings, refetch: refetchSettings } = useQuery({
    queryKey: ["user-settings"],
    queryFn: () => typedInvoke("get_user_settings"),
    select: (data) => data,
    refetchOnWindowFocus: true,
  });

  const { data: installedApplications = [] } = useQuery({
    queryKey: ["installed-applications"],
    queryFn: () => typedInvoke("list_installed_applications"),
    enabled: OS === "macos",
    staleTime: Infinity,
  });

  const { data: microphones = [], refetch: refetchMicrophones } = useQuery({
    queryKey: ["list_microphones"],
    queryFn: () => typedInvoke("list_microphones"),
    enabled: section === "audio",
    select: (devices) => devices.filter((device) => device.name !== "").sort((a, b) => a.name.localeCompare(b.name)),
  });
  const systemDefaultMic = microphones.find((device) => device.default)?.name;

  const { data: speakers = [], refetch: refetchSpeakers } = useQuery({
    queryKey: ["list_speakers"],
    queryFn: () => typedInvoke("list_speakers"),
    enabled: section === "audio",
    select: (devices) => devices.filter((device) => device.name !== "").sort((a, b) => a.name.localeCompare(b.name)),
  });
  const systemDefaultSpeaker = speakers.find((device) => device.default)?.name;

  const { data: drawPersist = false, refetch: refetchDrawPersist } = useQuery({
    queryKey: ["draw-persist"],
    queryFn: () => typedInvoke("get_sharer_draw_persist"),
    refetchOnWindowFocus: true,
  });

  useEffect(() => {
    if (settings) {
      setServerUrl(settings.hopp_server_url ?? "");
    }
  }, [settings]);

  async function commitShortcut(which: "mic" | "camera" | "screenshare" | "end_call", accel: string) {
    const setters = {
      mic: "set_shortcut_toggle_mic",
      camera: "set_shortcut_toggle_camera",
      screenshare: "set_shortcut_toggle_screenshare",
      end_call: "set_shortcut_end_call",
    } as const;

    if (settings) {
      const others = (["mic", "camera", "screenshare", "end_call"] as const).filter((k) => k !== which);
      for (const other of others) {
        const otherVal =
          other === "mic" ? settings.shortcut_toggle_mic
          : other === "camera" ? settings.shortcut_toggle_camera
          : other === "screenshare" ? settings.shortcut_toggle_screenshare
          : settings.shortcut_end_call;
        if (otherVal === accel) {
          await typedInvoke(setters[other], { accel: "" });
        }
      }
    }

    await typedInvoke(setters[which], { accel });
    refetchSettings();
  }

  async function saveServerUrl() {
    const trimmed = serverUrl.trim() || null;
    if (trimmed === (settings?.hopp_server_url ?? null)) return;
    await tauriUtils.setHoppServerUrl(trimmed);
    useStore.getState().setCustomServerUrl(trimmed);
    posthog.capture("custom_backend_url_changed");
    refetchSettings();
  }

  if (!settings) return null;

  const visibleSections = SECTIONS.filter((s) => !s.macOnly || OS === "macos");
  const current = visibleSections.find((s) => s.id === section) ?? visibleSections[0]!;

  return (
    <div className="flex h-full min-h-full overflow-hidden text-gray-900 dark:text-zinc-100">
      <div data-tauri-drag-region className="fixed inset-x-0 top-0 z-50 h-[32px]" />

      <nav className="flex w-[220px] shrink-0 flex-col gap-0.5 border-r border-black/5 bg-[#e9e9eb] px-3 pt-14 dark:border-white/5 dark:bg-[#29292b]">
        {visibleSections.map((s) => {
          const selected = s.id === current.id;
          return (
            <button
              key={s.id}
              type="button"
              onClick={() => setSection(s.id)}
              className={clsx(
                "flex items-center gap-2.5 rounded-lg px-2 py-1.5 text-left text-[13px]",
                selected ? "bg-blue-500 text-white" : (
                  "text-gray-800 hover:bg-black/5 dark:text-zinc-200 dark:hover:bg-white/5"
                ),
              )}
            >
              <span
                className={clsx(
                  "flex size-6 shrink-0 items-center justify-center rounded-md text-white [&>svg]:size-3.5",
                  s.tileClassName,
                )}
              >
                {s.icon}
              </span>
              {s.title}
            </button>
          );
        })}
      </nav>

      <main className="min-w-0 flex-1 overflow-y-auto bg-[#f5f5f7] dark:bg-[#1f1f21]">
        <div className="mx-auto flex max-w-[540px] flex-col gap-7 px-8 pb-12 pt-12">
          <PaneHeader
            icon={current.icon}
            tileClassName={current.tileClassName}
            title={current.title}
            subtitle={current.subtitle}
          />

          {current.id === "general" && (
            <>
              <SettingsGroup title="Behavior">
                <ToggleRow
                  title="Show dock icon when viewing a shared screen"
                  description="Switch to a teammate's shared screen with Cmd+Tab or the Dock"
                  checked={settings.show_dock_icon_in_call}
                  onCheckedChange={(v) => {
                    typedInvoke("set_show_dock_icon_in_call", { enabled: v }).then(() => refetchSettings());
                  }}
                />
                <ToggleRow
                  title="Notify me when I'm alone in a call"
                  description="After a minute on your own, and every 5 minutes after that"
                  checked={settings.notify_when_alone}
                  onCheckedChange={(v) => {
                    typedInvoke("set_notify_when_alone", { enabled: v }).then(() => refetchSettings());
                  }}
                />
                <ToggleRow
                  title="Call feedback popup"
                  description="Ask how the call went when it ends"
                  checked={settings.call_feedback_popup}
                  onCheckedChange={(v) => {
                    typedInvoke("set_call_feedback_popup", { enabled: v }).then(() => refetchSettings());
                  }}
                />
                {OS === "macos" && (
                  <ToggleRow
                    title="Automatic updates"
                    description="Download and install updates when you're not in a call"
                    checked={settings.auto_update_enabled}
                    onCheckedChange={(v) => {
                      typedInvoke("set_auto_update_enabled", { enabled: v }).then(() => refetchSettings());
                    }}
                  />
                )}
              </SettingsGroup>

              <SettingsGroup title="Privacy">
                <ToggleRow
                  title="Send anonymous telemetry"
                  description="Usage data and error reports help improve Hopp"
                  checked={settings.telemetry_enabled}
                  onCheckedChange={(v) => {
                    typedInvoke("set_telemetry_enabled", { enabled: v }).then(() => {
                      refetchSettings();
                      if (v) {
                        posthog.opt_in_capturing();
                      } else {
                        posthog.opt_out_capturing();
                      }
                    });
                  }}
                />
              </SettingsGroup>

              <SettingsGroup title="Advanced">
                <SettingRow title="Server" description="Leave empty to use the default server" htmlFor="server-url">
                  <Input
                    id="server-url"
                    type="text"
                    placeholder={URLS.API_BASE_URL}
                    value={serverUrl}
                    className="h-8 w-[220px] text-[13px] dark:bg-zinc-900/40"
                    onChange={(e) => setServerUrl(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") void saveServerUrl();
                    }}
                    onBlur={() => void saveServerUrl()}
                  />
                </SettingRow>
              </SettingsGroup>
            </>
          )}

          {current.id === "audio" && (
            <>
              <SettingsGroup>
                <SettingRow
                  title="Output device"
                  description="System default follows your Mac's sound output, also during a call"
                  htmlFor="audio-output"
                >
                  <Select
                    value={settings.audio_output != null ? `speaker:${settings.audio_output}` : "system-default"}
                    onOpenChange={(open) => {
                      if (open) refetchSpeakers();
                    }}
                    onValueChange={(value) => {
                      const deviceName = value.startsWith("speaker:") ? value.slice("speaker:".length) : null;
                      typedInvoke("set_audio_output", { deviceName }).then(() => refetchSettings());
                    }}
                  >
                    <SelectTrigger id="audio-output" className={rowSelectClassName}>
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="system-default">
                        {systemDefaultSpeaker ? `System default (${systemDefaultSpeaker})` : "System default"}
                      </SelectItem>
                      {settings.audio_output && !speakers.some((device) => device.name === settings.audio_output) && (
                        <SelectItem value={`speaker:${settings.audio_output}`} disabled>
                          {settings.audio_output} (Unavailable)
                        </SelectItem>
                      )}
                      {speakers.map((device) => (
                        <SelectItem key={device.name} value={`speaker:${device.name}`}>
                          {device.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </SettingRow>
                <SettingRow
                  title="Input device"
                  description="System default follows your Mac's input setting, also during a call"
                  htmlFor="default-microphone"
                >
                  <Select
                    value={
                      settings.default_mic != null ? `mic:${settings.default_mic}`
                      : settings.use_last_used_mic ?
                        "last-used"
                      : "system-default"
                    }
                    disabled={savingDefaultMic}
                    onOpenChange={(open) => {
                      if (open) refetchMicrophones();
                    }}
                    onValueChange={async (value) => {
                      setSavingDefaultMic(true);
                      setDefaultMicError("");
                      try {
                        await typedInvoke("set_default_mic", {
                          deviceName: value.startsWith("mic:") ? value.slice(4) : null,
                          useLastUsed: value === "last-used",
                        });
                        await refetchSettings();
                      } catch (error) {
                        setDefaultMicError(String(error));
                      } finally {
                        setSavingDefaultMic(false);
                      }
                    }}
                  >
                    <SelectTrigger id="default-microphone" className={rowSelectClassName}>
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="system-default">
                        {systemDefaultMic ? `System default (${systemDefaultMic})` : "System default"}
                      </SelectItem>
                      <SelectItem value="last-used">Last-used microphone</SelectItem>
                      {settings.default_mic && !microphones.some((device) => device.name === settings.default_mic) && (
                        <SelectItem value={`mic:${settings.default_mic}`} disabled>
                          {settings.default_mic} (Unavailable)
                        </SelectItem>
                      )}
                      {microphones.map((device) => (
                        <SelectItem key={device.name} value={`mic:${device.name}`}>
                          {device.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </SettingRow>
                {defaultMicError && (
                  <p role="alert" className="py-2 text-xs text-red-600 dark:text-red-400">
                    {defaultMicError}
                  </p>
                )}
              </SettingsGroup>

              <SettingsGroup>
                <ToggleRow
                  title="Automatically adjust my microphone volume"
                  description="Moves your Mac's input volume during calls so you're heard at a steady level"
                  checked={settings.auto_mic_volume}
                  onCheckedChange={(v) => {
                    typedInvoke("set_auto_mic_volume", { enabled: v }).then(() => refetchSettings());
                  }}
                />
                <ToggleRow
                  title="Suppress my background noise"
                  checked={settings.noise_cancellation_enabled}
                  onCheckedChange={(v) => {
                    typedInvoke("set_noise_cancellation", { enabled: v }).then(() => refetchSettings());
                  }}
                />
              </SettingsGroup>

              <SettingsGroup title="Calls">
                <ToggleRow
                  title="Start muted"
                  description="Your microphone stays off until you unmute"
                  checked={!settings.start_mic_on_call}
                  onCheckedChange={(v) => {
                    typedInvoke("set_start_mic_on_call", { enabled: !v }).then(() => refetchSettings());
                  }}
                />
              </SettingsGroup>
            </>
          )}

          {current.id === "webcam" && (
            <SettingsGroup title="Behavior">
              <ToggleRow
                title="Enable my webcam automatically"
                description="Turn your camera on when a call starts"
                checked={settings.start_camera_on_call}
                onCheckedChange={(v) => {
                  typedInvoke("set_start_camera_on_call", { enabled: v }).then(() => refetchSettings());
                }}
              />
              <ToggleRow
                title="Show preview before sharing"
                description="Check how you look first. Only you see the preview until you click Share webcam."
                checked={settings.webcam_preview}
                onCheckedChange={(v) => {
                  typedInvoke("set_webcam_preview", { enabled: v }).then(() => refetchSettings());
                }}
              />
            </SettingsGroup>
          )}

          {current.id === "screen-share" && (
            <>
              <SettingsGroup title="Sharing">
                <SelectRow
                  id="screen-share-resolution"
                  title="Resolution"
                  description="Higher resolutions need a faster connection"
                  value={settings.screen_share_resolution}
                  items={screenShareResolutionItems}
                  onValueChange={(resolution) => {
                    typedInvoke("set_screen_share_resolution", { resolution }).then(() => refetchSettings());
                  }}
                />
                {OS === "macos" && (
                  <SelectRow
                    id="screen-share-picker-mode"
                    title="Pick by default"
                    value={settings.screen_share_picker_mode}
                    items={screenSharePickerModeItems}
                    onValueChange={(mode) => {
                      typedInvoke("set_screen_share_picker_mode", { mode }).then(() => refetchSettings());
                    }}
                  />
                )}
                <ToggleRow
                  title="Let pairs control my screen"
                  description="Remote control is on when you start sharing"
                  checked={settings.remote_control_enabled}
                  onCheckedChange={(v) => {
                    typedInvoke("set_remote_control_enabled", { enabled: v }).then(() => refetchSettings());
                  }}
                />
              </SettingsGroup>

              <SettingsGroup title="Drawing">
                <ToggleRow
                  title="Keep drawings until right-click"
                  description="Lines stay on screen instead of fading after a few seconds"
                  checked={drawPersist}
                  onCheckedChange={(v) => {
                    typedInvoke("set_draw_persist", { persist: v }).then(() => refetchDrawPersist());
                  }}
                />
              </SettingsGroup>
            </>
          )}

          {current.id === "app-veil" && OS === "macos" && (
            <AppVeilSettings
              rows={settings.app_veil_applications}
              installedApplications={installedApplications}
              onChange={async (applications) => {
                try {
                  await typedInvoke("set_app_veil_applications", { applications });
                } finally {
                  await refetchSettings();
                }
              }}
            />
          )}

          {current.id === "hotkeys" && (
            <SettingsGroup>
              <ShortcutRow
                title="Mute"
                description="Turn your microphone on or off"
                value={settings.shortcut_toggle_mic}
                onCommit={(accel) => commitShortcut("mic", accel)}
              />
              <ShortcutRow
                title="Webcam"
                description="Turn your camera on or off"
                value={settings.shortcut_toggle_camera}
                onCommit={(accel) => commitShortcut("camera", accel)}
              />
              <ShortcutRow
                title="Screen share"
                description="Start or stop sharing your screen"
                value={settings.shortcut_toggle_screenshare}
                onCommit={(accel) => commitShortcut("screenshare", accel)}
              />
              <ShortcutRow
                title="Hang up"
                description="Leave the current call"
                value={settings.shortcut_end_call}
                onCommit={(accel) => commitShortcut("end_call", accel)}
              />
            </SettingsGroup>
          )}
        </div>
      </main>
    </div>
  );
}
