// Reveals a scroll container's thumb only while it's actively scrolling
// (native macOS overlay feel) instead of always-on. Scroll events don't
// bubble, so this listens once in the capture phase at the document
// root rather than wiring a handler into each of the app's many
// overflow-y containers.
const hideTimers = new WeakMap<Element, ReturnType<typeof setTimeout>>();

function scrolledElement(target: EventTarget | null): Element | null {
  if (target instanceof Element) return target;
  if (target instanceof Document) return target.scrollingElement;
  return null;
}

export function installScrollbarAutoHide() {
  document.addEventListener(
    "scroll",
    (e) => {
      const el = scrolledElement(e.target);
      if (!el) return;
      el.classList.add("is-scrolling");
      clearTimeout(hideTimers.get(el));
      hideTimers.set(
        el,
        setTimeout(() => el.classList.remove("is-scrolling"), 500),
      );
    },
    { capture: true, passive: true },
  );
}
