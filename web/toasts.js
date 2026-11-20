import {
  createNotificationsStore,
  notifications,
  updateNotificationsState,
} from "@mantine/notifications";

// One owner for result feedback: bounded queue, replacement, dismissal and timers.
// Rendering and animation remain with Mantine's notification components.
export function createToastManager() {
  const store = createNotificationsStore();
  store.setState({ ...store.getState(), limit: 3 });
  let sequence = 0;
  function show(message, { kind = "info", id, autoClose } = {}) {
    if (!message) return null;
    const all = [...store.getState().notifications, ...store.getState().queue];
    const existing = all.find((n) =>
      id ? n.id === id : n.message === message && n["data-kind"] === kind,
    );
    const data = {
      id: existing?.id ?? id ?? `toast-${++sequence}`,
      message,
      color: { info: "blue", success: "teal", error: "red", warning: "orange" }[
        kind
      ],
      autoClose:
        autoClose ?? (kind === "error" || kind === "warning" ? 8000 : 4000),
      withBorder: true,
      radius: "lg",
      className: "app-toast",
      role: kind === "error" || kind === "warning" ? "alert" : "status",
      "aria-live": kind === "error" ? "assertive" : "polite",
      "data-kind": kind,
      closeButtonProps: { "aria-label": "关闭通知" },
    };
    if (existing) notifications.update(data, store);
    else notifications.show(data, store);
    updateNotificationsState(store, (items) => items.slice(-12));
    return data.id;
  }
  return {
    store,
    show,
    info: (message, options) => show(message, { ...options, kind: "info" }),
    success: (message, options) =>
      show(message, { ...options, kind: "success" }),
    error: (message, options) => show(message, { ...options, kind: "error" }),
    warning: (message, options) =>
      show(message, { ...options, kind: "warning" }),
    dismiss: (id) => notifications.hide(id, store),
    clear: () => notifications.clean(store),
  };
}

export const toast = createToastManager();
