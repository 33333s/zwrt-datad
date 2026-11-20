// LAN HTTP pages may not expose the secure-context Clipboard API.
export async function copyText(text) {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      /* Try the user-gesture fallback. */
    }
  }
  const previous = document.activeElement,
    input = document.createElement("textarea");
  const parent = previous?.closest?.('[role="dialog"]') ?? document.body;
  input.value = text;
  input.setAttribute("aria-hidden", "true");
  Object.assign(input.style, {
    position: "fixed",
    top: "0",
    left: "0",
    width: "1px",
    height: "1px",
    opacity: "0",
  });
  parent.append(input);
  try {
    input.focus({ preventScroll: true });
    input.select();
    if (!document.execCommand("copy")) throw new Error("浏览器未允许复制");
  } finally {
    input.remove();
    previous?.focus?.({ preventScroll: true });
  }
}
