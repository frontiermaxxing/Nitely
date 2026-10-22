# nitely.net

The public site: one static page with its stylesheet and assets, and no build step. It uses the
app's own design tokens (`src/styles/tokens.css`) and the Geist fonts served from here, so the
page's own requests stay on nitely.net. The one exception is Google Analytics: GA4 property
`nitely.net`, measurement ID `G-E79GE90HZG`, in a GA account of its own. Its tag is inline in each
page's head, and a new page copies it.

- `index.html` and `styles.css`: the page. Dark first, light follows the system, and the hero track
  stops moving under reduced motion.
- `assets/`: the fonts (OFL-1.1, from `@fontsource-variable/geist`), the app icon, and screenshots
  of the app on its sample data (`src/dev/fixtures.ts`), never a real project's.

To preview it, serve this folder (`python -m http.server 8099` in `site/`) and open
http://127.0.0.1:8099.

To refresh the screenshots, run `npm run dev` and capture `/?theme=dark#overview` and `#inbox` at
1440x900 and 2x, in each theme. Crop the sidebar's preview-only block off the bottom, and save them
as WebP.

To publish it, copy the folder's contents to the web root behind nitely.net. It needs only a static
file server with TLS, and long cache headers on `assets/`.

Cloudflare caches the stylesheet and scripts for hours. After changing `styles.css`, raise the `?v=` number on its links in both pages, so visitors get the new file
at once.

`leaderboard/index.html` (its script is inline, so the repo's test map has no loose JavaScript to own) reads the public board from `/v1/leaderboard/board/{week,30d,all}.json` on this
same site, so the web server proxies that one path (GET only) to the Leaderboard service
(`server/leaderboard`). It renders every value as text and shows a link only when it is https.

`u/index.html` is the page for one person, `/u/<id>`. The Leaderboard service serves it, not
this folder: it is compiled into the service, which fills in `{{head}}` (title, summary and link
preview tags) and `{{person}}` (their numbers), so the web server sends `/u/` to the service
(server/leaderboard/README.md, "Deploying people's pages"). Its links are absolute (`/styles.css`)
because it is served from `/u/<id>`; raise its `?v=` with the other pages'.
