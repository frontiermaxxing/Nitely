// The phone page's service worker (docs/phone.md, "Notifications"): shows the notifications the
// Nitely app pushes when a decision waits, and opens the page when one is tapped. It holds no key
// and makes no request of its own.

self.addEventListener("push", (event) => {
  let message = {};
  try {
    message = event.data ? event.data.json() : {};
  } catch {
    message = {};
  }
  event.waitUntil(
    self.registration.showNotification(message.title || "Nitely", {
      body: message.body || "A decision waits for you.",
      // One notification at a time: a newer one replaces the last.
      tag: "nitely-decisions",
      renotify: true,
      icon: "/phone/icon-192.png",
      data: { url: "/phone/" },
    }),
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  event.waitUntil(
    (async () => {
      const open = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      const page = open.find((c) => new URL(c.url).pathname.startsWith("/phone"));
      if (page) return page.focus();
      return self.clients.openWindow("/phone/");
    })(),
  );
});
