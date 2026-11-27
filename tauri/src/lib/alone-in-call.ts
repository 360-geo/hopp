import { useEffect } from "react";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import useStore, { remoteUserIds } from "@/store/store";
import { typedInvoke } from "@/core_payloads";

/** The first reminder comes after a minute alone, then one every five minutes. */
const FIRST_REMINDER_MS = 60_000;
const REPEAT_REMINDER_MS = 5 * 60_000;

async function remindAlone(roomName: string | undefined) {
  try {
    const settings = await typedInvoke("get_user_settings");
    if (!settings.notify_when_alone) return;
    let granted = await isPermissionGranted();
    if (!granted) granted = (await requestPermission()) === "granted";
    if (!granted) return;
    sendNotification({
      title: "You're alone in the call",
      body: `Nobody else is in ${roomName ?? "your call"}.`,
    });
  } catch (error) {
    console.error("Failed to send the alone-in-call reminder", error);
  }
}

/**
 * Sends a notification while the user is the only one left in a call, so a
 * forgotten call doesn't keep their microphone open. Waits until the call has
 * connected: an outgoing call that is still ringing doesn't count.
 */
export function useAloneInCallReminder() {
  const alone = useStore((state) => {
    const call = state.callTokens;
    if (!call || call.connectingTo || call.isInitialisingCall) return false;
    return remoteUserIds(call.participants).size === 0;
  });
  const roomName = useStore((state) => state.callTokens?.room?.name);

  useEffect(() => {
    if (!alone) return;
    let repeat: ReturnType<typeof setInterval> | undefined;
    const first = setTimeout(() => {
      void remindAlone(roomName);
      repeat = setInterval(() => void remindAlone(roomName), REPEAT_REMINDER_MS);
    }, FIRST_REMINDER_MS);
    return () => {
      clearTimeout(first);
      if (repeat) clearInterval(repeat);
    };
  }, [alone, roomName]);
}
