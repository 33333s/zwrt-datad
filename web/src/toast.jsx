import React from "react";
import { Notifications } from "@mantine/notifications";
import { useMediaQuery } from "@mantine/hooks";
import { useReducedMotion } from "motion/react";
import { toast } from "../toasts.js";

export function ToastHost() {
  const mobile = useMediaQuery("(max-width: 48em)"),
    reduced = useReducedMotion();
  return (
    <Notifications
      store={toast.store}
      position={mobile ? "top-center" : "top-right"}
      limit={3}
      zIndex={1200}
      containerWidth={380}
      transitionDuration={reduced ? 0 : 180}
      pauseResetOnHover="notification"
      className="app-toast-host"
    />
  );
}
